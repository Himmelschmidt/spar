#!/usr/bin/env bash
# End-to-end check of the spar tools in real pi, with no billed model and no real agents.
# Usage: test/e2e/run.sh <project> <sandbox>
#   <project>  an initialized spar project; only read-only spar commands run there
#   <sandbox>  a path that must not exist; a scratch git repo is made there for launches
#              (SPAR_DRY_RUN=1, so nothing spawns or bills) and deleted at the end
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
pkg=$(cd "$here/../.." && pwd)
project=$(cd "$1" && pwd)
sandbox=$2
if [ -e "$sandbox" ]; then
	echo "refusing: $sandbox exists" >&2
	exit 1
fi
unset SPAR_PROJECT_ROOT SPAR_RUN_ID SPAR_AGENT_ID SPAR_PI_UNATTENDED_LAUNCH
spar=${SPAR_PI_BIN:-spar}
common=(--offline --no-session --no-extensions --no-context-files -e "$pkg" -e "$here/scripted-provider.ts" --model scripted/scripted)
step() { printf '\n===== %s\n' "$1"; }

step "read-only tools in $project"
run=$(cd "$project" && "$spar" status --json | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const r=JSON.parse(s);console.log((r.find(x=>x.workflow==="loop")??r[0]).id)})')
export SPAR_PI_SCRIPT='[[{"name":"spar_status","arguments":{}},{"name":"spar_status","arguments":{"run":"'$run'"}},{"name":"spar_brief","arguments":{"run":"'$run'"}}],[{"name":"spar_events","arguments":{"run":"'$run'","limit":5}},{"name":"spar_logs","arguments":{"run":"'$run'","lines":5}},{"name":"spar_bus_log","arguments":{"run":"'$run'","limit":3}}],[{"name":"spar_providers","arguments":{}},{"name":"spar_stats","arguments":{"since":"60d"}},{"name":"spar_skill","arguments":{}}]]'
(cd "$project" && pi "${common[@]}" -p "look at spar")

mkdir -p "$sandbox"
trap 'rm -rf "$sandbox"' EXIT
git -C "$sandbox" init -q -b main
echo hello >"$sandbox/a.txt"
git -C "$sandbox" add a.txt
git -C "$sandbox" -c user.name=e2e -c user.email=e2e@localhost -c commit.gpgsign=false commit -qm init
export SPAR_DRY_RUN=1
cd "$sandbox"

step "launch with no UI and no unattended flag fails closed"
export SPAR_PI_SCRIPT='[[{"name":"spar_plan","arguments":{"task":"add a second line","providers":["cli:claude"],"fleet":"small","dry_run":true}}]]'
pi "${common[@]}" -p "plan it"
echo "runs after: $("$spar" status --json)"

step "rpc mode: the operator declines one launch, accepts the next"
export SPAR_PI_SCRIPT='[[{"name":"spar_plan","arguments":{"task":"add a second line","providers":["cli:claude"],"fleet":"small","dry_run":true}}],[{"name":"spar_plan","arguments":{"task":"add a second line","providers":["cli:claude"],"roles":["reviewer=cli:codex"],"fleet":"small","dry_run":true}}],[{"name":"spar_wait","arguments":{"run":"$LAST_RUN","timeout_minutes":1}},{"name":"spar_brief","arguments":{"run":"$LAST_RUN"}}],[{"name":"spar_bus_send","arguments":{"run":"$LAST_RUN","message":"note from pi"}}],[{"name":"spar_bus_log","arguments":{"run":"$LAST_RUN","limit":2}},{"name":"spar_events","arguments":{"run":"$LAST_RUN","limit":4}}]]'
SPAR_PI_CONFIRMS=no,yes node "$here/rpc-confirm.ts" "${common[@]}"

step "unattended flag: implement and review launch without a dialog"
export SPAR_PI_SCRIPT='[[{"name":"spar_implement","arguments":{"task":"add a third line","providers":["cli:claude"],"fleet":"small","dry_run":true}}],[{"name":"spar_review","arguments":{"task":"review the last commit","providers":["cli:claude","cli:codex"],"dry_run":true}}]]'
pi "${common[@]}" --spar-unattended-launch -p "build it"
echo "runs after: $("$spar" status --json | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).map(r=>`${r.id} ${r.workflow} ${r.phase} dry_run=${r.dry_run}`).join("; ")))')"
