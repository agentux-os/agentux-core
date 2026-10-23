#!/bin/sh
# One full run (plan -> implement -> gate -> review -> pull request) with a
# real agent CLI on a copy of tests/e2e/sandbox, through agentuxd and aux.
#
#   tests/e2e/run-real.sh <harness> <aux binary dir>
#
# The harness must be signed in already (an API key in the environment, or
# its login files). With AGENTUX_E2E_REPO (owner/name) and GH_TOKEN the run
# opens a draft pull request there, which is closed (and its branch deleted)
# afterwards; without them the pipeline stops after the review. Never prints
# credentials: they stay in the environment of the processes that need them.
set -eu

harness=${1:?usage: run-real.sh <harness> <bin dir>}
bin=${2:?usage: run-real.sh <harness> <bin dir>}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
export XDG_RUNTIME_DIR="$work/run" XDG_STATE_HOME="$work/state"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME"
export PATH="$bin:$PATH"

# The project: the sandbox, in a git repository of its own.
project="$work/project"
branch="agentux-e2e-$harness-$(date -u +%Y%m%d%H%M%S)"
if [ -n "${AGENTUX_E2E_REPO:-}" ] && [ -n "${GH_TOKEN:-}" ]; then
    git clone --quiet "https://x-access-token:${GH_TOKEN}@github.com/${AGENTUX_E2E_REPO}.git" "$project"
    git -C "$project" checkout --quiet -b "$branch"
    pull_request="  - step: pull_request
    draft: true
    approve: false"
else
    git init --quiet -b main "$project"
    pull_request=""
fi
cp "$here/sandbox/"* "$project/"
check="python3 -m unittest -v"
[ "${AGENTUX_E2E_FAKE:-}" = 1 ] && check=true
cat >"$project/agentux.yaml" <<EOF
version: 1
roles:
  planner:
    harness: $harness
  implementer:
    harness: $harness
  reviewer:
    harness: $harness
checks:
  - name: test
    run: ${check}
    timeout: 2m
pipeline:
  - step: plan
    role: planner
    approve: false
  - step: implement
    role: implementer
  - step: gate
    checks: [test]
    on_fail: implement
    max_attempts: 3
  - step: review
    role: reviewer
    on_changes_requested: implement
    max_rounds: 1
$pull_request
budget:
  max_usd_per_run: 3
EOF
git -C "$project" add -A
git -C "$project" -c user.name=agentux-e2e -c user.email=e2e@agentuxos.org commit --quiet -m "AgentUX end-to-end sandbox"
if [ -n "$pull_request" ]; then
    git -C "$project" push --quiet origin "$branch"
fi

# The daemon: unattended, so tool calls are approved (this is a sandbox).
# AGENTUX_E2E_FAKE=1 answers with scripted agents instead: the plumbing,
# without credentials (the gate then only runs `true`).
fake=""
[ "${AGENTUX_E2E_FAKE:-}" = 1 ] && fake=--fake-agents
aux daemon --auto-approve-permissions $fake >"$work/agentuxd.log" 2>&1 &
daemon=$!
cleanup() {
    kill "$daemon" 2>/dev/null || true
    if [ -n "$pull_request" ]; then
        gh pr list -R "$AGENTUX_E2E_REPO" --head "$branch" --json number -q '.[].number' |
            while read -r pr; do gh pr close -R "$AGENTUX_E2E_REPO" "$pr" --delete-branch >/dev/null || true; done
        git -C "$project" push --quiet origin --delete "$branch" 2>/dev/null || true
    fi
}
trap cleanup EXIT
for _ in $(seq 60); do
    [ -S "$XDG_RUNTIME_DIR/agentux/agentuxd.sock" ] && break
    sleep 1
done
aux agents

echo "--- $harness: a full run"
status=0
timeout 45m aux run "$project" --watch \
    --prompt "The tests in test_calc.py fail. Fix calc.py so they pass, without changing the tests." || status=$?

aux ps --all --json >"$work/status.json" || true
cp "$work/status.json" "$work/agentuxd.log" "${AGENTUX_E2E_OUT:-.}/" 2>/dev/null || true
if [ -n "$pull_request" ] && [ "$status" = 0 ]; then
    pr=$(gh pr list -R "$AGENTUX_E2E_REPO" --head "$branch" --json url -q '.[0].url')
    [ -n "$pr" ] || { echo "the run ended, but no pull request was opened" >&2; exit 1; }
    echo "pull request: $pr"
fi
exit "$status"
