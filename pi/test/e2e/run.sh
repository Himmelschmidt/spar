#!/usr/bin/env bash
# End-to-end check of the spar tools in real pi, with no billed model and no real agents.
# Usage: test/e2e/run.sh <project> <sandbox>
#   <project>  an initialized spar project; only read-only spar commands run there
#   <sandbox>  a path that must not exist; a scratch git repo is made there for launches
#              (SPAR_DRY_RUN=1, so nothing spawns or bills) and deleted at the end
# PI_BIN picks pi (default: pi on PATH, which must be 1.4.0); SPAR_PI_BIN picks spar.
# Exits non-zero on the first expectation that does not hold.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
pkg=$(cd "$here/../.." && pwd)
project=$(cd "$1" && pwd)
sandbox=$2
if [ -e "$sandbox" ]; then
	echo "refusing: $sandbox exists" >&2
	exit 1
fi
export PI_BIN=${PI_BIN:-pi}
version=$("$PI_BIN" --version 2>&1 | tail -1)
if [ "$version" != "1.4.0" ]; then
	echo "FAIL: $PI_BIN is pi $version, these checks were written against 1.4.0" >&2
	exit 1
fi
unset SPAR_PROJECT_ROOT SPAR_RUN_ID SPAR_AGENT_ID SPAR_PI_UNATTENDED_LAUNCH
spar=${SPAR_PI_BIN:-spar}
out=$(mktemp -d)
mkdir -p "$sandbox"
trap 'rm -rf "$sandbox" "$out"' EXIT
common=(--offline --no-session --no-extensions --no-context-files -e "$pkg" -e "$here/scripted-provider.ts" --model scripted/scripted)
failed=0
step() { printf '\n===== %s\n' "$1"; }
expect() {
	if grep -qF -- "$2" "$1"; then
		echo "ok: $2"
	else
		echo "FAIL: expected \"$2\" in $3"
		failed=1
	fi
}
reject() {
	if grep -qF -- "$2" "$1"; then
		echo "FAIL: did not expect \"$2\" in $3"
		failed=1
	else
		echo "ok: no \"$2\""
	fi
}
equal() {
	if [ "$1" = "$2" ]; then echo "ok: $3 = $2"; else
		echo "FAIL: $3 is \"$1\", expected \"$2\""
		failed=1
	fi
}
runs() { "$spar" status --json | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).map(r=>`${r.workflow}:${r.phase}:${r.dry_run}`).sort().join(" ")))'; }

step "read-only tools in $project"
run=$(cd "$project" && "$spar" status --json | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const r=JSON.parse(s);console.log((r.find(x=>x.workflow==="loop")??r[0]).id)})')
export SPAR_PI_SCRIPT='[[{"name":"spar_status","arguments":{}},{"name":"spar_status","arguments":{"run":"'$run'"}},{"name":"spar_brief","arguments":{"run":"'$run'"}}],[{"name":"spar_events","arguments":{"run":"'$run'","limit":5}},{"name":"spar_logs","arguments":{"run":"'$run'","lines":5}},{"name":"spar_bus_log","arguments":{"run":"'$run'","limit":3}}],[{"name":"spar_providers","arguments":{}},{"name":"spar_stats","arguments":{"since":"60d"}},{"name":"spar_skill","arguments":{}}]]'
(cd "$project" && "$PI_BIN" "${common[@]}" -p "look at spar" </dev/null) | tee "$out/read.txt"
expect "$out/read.txt" "spar skill advertised: true" read
expect "$out/read.txt" "spar tools declared: spar_status,spar_brief,spar_events,spar_logs,spar_providers,spar_stats,spar_plan,spar_implement,spar_review,spar_wait,spar_bus_send,spar_bus_log,spar_skill" read
for tool in spar_status spar_brief spar_events spar_logs spar_bus_log spar_providers spar_stats spar_skill; do
	expect "$out/read.txt" "## $tool"$'\n' read
done
reject "$out/read.txt" "(error)" read
expect "$out/read.txt" "run $run " read
expect "$out/read.txt" "next: " read
expect "$out/read.txt" "Sections (call spar_skill" read

git -C "$sandbox" init -q -b main
echo hello >"$sandbox/a.txt"
git -C "$sandbox" add a.txt
git -C "$sandbox" -c user.name=e2e -c user.email=e2e@localhost -c commit.gpgsign=false commit -qm init
export SPAR_DRY_RUN=1
# A spar home of its own keeps the sandbox out of the operator's project registry, so the
# operator's daemon never supervises it or writes into it after it is deleted.
export SPAR_HOME="$out/spar-home"
cd "$sandbox"

step "no UI: a launch and a bash gate command both fail closed"
export SPAR_PI_SCRIPT='[[{"name":"spar_plan","arguments":{"task":"add a second line","providers":["cli:claude"],"fleet":"small","dry_run":true}},{"name":"bash","arguments":{"command":"cd . && spar approve abcd1234"}}]]'
"$PI_BIN" "${common[@]}" -p "plan it" </dev/null | tee "$out/noui.txt"
expect "$out/noui.txt" "not launched: no operator UI to confirm a launch" noui
expect "$out/noui.txt" "## bash (error)" noui
expect "$out/noui.txt" "blocked: this bash command runs spar an operator action" noui
equal "$(runs)" "" "runs after"

step "rpc: the operator cancels one launch, launches the next, refuses a bash approve"
export SPAR_PI_SCRIPT='[[{"name":"spar_plan","arguments":{"task":"add a second line","providers":["cli:claude"],"fleet":"small","dry_run":true}}],[{"name":"spar_plan","arguments":{"task":"add a second line","providers":["cli:claude"],"roles":["reviewer=cli:codex"],"fleet":"small","dry_run":true}}],[{"name":"spar_wait","arguments":{"run":"$LAST_RUN","timeout_minutes":1}},{"name":"spar_brief","arguments":{"run":"$LAST_RUN"}}],[{"name":"bash","arguments":{"command":"spar approve $LAST_RUN"}}],[{"name":"spar_implement","arguments":{"run":"$LAST_RUN"}}],[{"name":"spar_bus_send","arguments":{"run":"$LAST_RUN","message":"note from pi"}}],[{"name":"spar_bus_log","arguments":{"run":"$LAST_RUN","limit":2}},{"name":"spar_events","arguments":{"run":"$LAST_RUN","limit":4}}]]'
SPAR_PI_ANSWERS=Cancel,Launch,Cancel node "$here/rpc-confirm.ts" "${common[@]}" | tee "$out/rpc.txt"
expect "$out/rpc.txt" "--- options: Cancel | Launch | View full task" rpc
expect "$out/rpc.txt" "Gates: plan approval and ship wait for you (autonomy manual)" rpc
expect "$out/rpc.txt" "  planner cli:claude  [planner-cli-claude, cli-providers]" rpc
expect "$out/rpc.txt" "  reviewer cli:codex (later phase)  [review-0-cli-codex, cli-role]" rpc
expect "$out/rpc.txt" "not launched: the operator declined the launch" rpc
expect "$out/rpc.txt" "exit 2: waiting on the human at a gate (awaiting_plan_approval)" rpc
expect "$out/rpc.txt" "The model wants to run spar through bash: an operator action" rpc
expect "$out/rpc.txt" "the operator declined this spar command" rpc
expect "$out/rpc.txt" "refused: run " rpc
expect "$out/rpc.txt" "is waiting on the operator at awaiting_plan_approval" rpc
expect "$out/rpc.txt" "sent " rpc
expect "$out/rpc.txt" "pi -> broadcast: note from pi" rpc
equal "$(runs)" "plan:awaiting_plan_approval:true" "runs after"

step "unattended flag: implement and review launch without a dialog"
export SPAR_PI_SCRIPT='[[{"name":"spar_implement","arguments":{"task":"add a third line","providers":["cli:claude"],"fleet":"small","dry_run":true}}],[{"name":"spar_wait","arguments":{"run":"$LAST_RUN","timeout_minutes":2}}],[{"name":"spar_review","arguments":{"task":"review the last commit","providers":["cli:claude","cli:codex"],"dry_run":true}}]]'
"$PI_BIN" "${common[@]}" --spar-unattended-launch -p "build it" </dev/null | tee "$out/unattended.txt"
expect "$out/unattended.txt" "## spar_implement"$'\n' unattended
expect "$out/unattended.txt" "## spar_review"$'\n' unattended
reject "$out/unattended.txt" "(error)" unattended
expect "$out/unattended.txt" "--key=pi-" unattended
equal "$(runs)" "loop:awaiting_ship_confirm:true plan:awaiting_plan_approval:true review:done:true" "runs after"

if [ "$failed" != 0 ]; then
	echo $'\nE2E FAILED'
	exit 1
fi
echo $'\nE2E PASSED'
