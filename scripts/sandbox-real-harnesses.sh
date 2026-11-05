#!/usr/bin/env bash
# The agent sandbox (ADRs 0023 and 0024) with your real, signed-in agent
# CLIs: one run per harness in a throwaway project whose agentux.yaml asks
# for `isolation.agents.mode: bubblewrap`, on an agentuxd of its own (its
# own socket and database, so your daemon and runs are not touched).
#
#   scripts/sandbox-real-harnesses.sh [--harness NAME]... [--keep] [--bin DIR]
#
# Each harness is asked to create a file in its worktree and to try writing
# two files outside it (in $HOME and in the project's main checkout). The
# script then checks that the session ran sandboxed (`agentIsolation`), with
# that harness, finished, made the file, and that nothing landed outside.
# One harness is then run again with `network: none`, which must fail with
# a message instead of hanging.
#
# Harnesses that are not installed or not signed in are skipped. Nothing it
# prints comes from your environment or the CLIs' files; logs go to a
# directory named at the end. The daemon's log holds the agents' stderr, so
# review it before sharing.
#
# --fake runs the same checks with scripted agents and a throwaway $HOME (no
# credentials, nothing of yours involved), through the real sandbox: CI uses
# it to prove the script works.
set -euo pipefail

usage() {
    cat <<'EOF'
usage: sandbox-real-harnesses.sh [options]

  --harness NAME    check this harness (repeatable): claude-code, codex,
                    opencode, antigravity (experimental) [default: claude-code
                    codex opencode]
  --offline NAME    the harness to run with `network: none` [default: the
                    first one that passed]
  --no-offline      skip the `network: none` run
  --timeout MIN     minutes each run may take [default: 20; 3 with --fake]
  --bin DIR         use aux and agentuxd from DIR (e.g. target/release)
  --user-config     keep your ~/.config/agentux (preferences, tool catalog,
                    sandbox.yaml) instead of an empty one
  --keep            keep the throwaway projects and worktrees
  --fake            scripted agents and a throwaway $HOME (for CI)
EOF
}

harnesses=()
offline=""
no_offline=0
timeout_min=""
bin=""
user_config=0
keep=0
fake=0
while [ $# -gt 0 ]; do
    case "$1" in
        --harness) harnesses+=("${2:?--harness needs a name}"); shift 2 ;;
        --offline) offline=${2:?--offline needs a name}; shift 2 ;;
        --no-offline) no_offline=1; shift ;;
        --timeout) timeout_min=${2:?--timeout needs minutes}; shift 2 ;;
        --bin) bin=${2:?--bin needs a directory}; shift 2 ;;
        --user-config) user_config=1; shift ;;
        --keep) keep=1; shift ;;
        --fake) fake=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
if [ ${#harnesses[@]} -eq 0 ]; then
    harnesses=(claude-code codex opencode)
    # The scripted run also covers the refusal of an unsupported harness.
    [ "$fake" = 1 ] && harnesses+=(antigravity)
fi
if [ -z "$timeout_min" ]; then
    if [ "$fake" = 1 ]; then timeout_min=3; else timeout_min=20; fi
fi
if [ -n "$bin" ]; then
    bin=$(cd "$bin" && pwd)
    PATH="$bin:$PATH"
fi

if [ "$(uname -s)" != Linux ]; then
    echo "the agent sandbox is Linux-only (Bubblewrap)" >&2
    exit 2
fi
missing=()
for program in aux git jq timeout bwrap pasta; do
    command -v "$program" >/dev/null || missing+=("$program")
done
[ "$fake" = 1 ] && { command -v python3 >/dev/null || missing+=(python3); }
if [ ${#missing[@]} -gt 0 ]; then
    echo "missing: ${missing[*]} (bwrap is the bubblewrap package, pasta the passt package)" >&2
    exit 2
fi

# Physical paths: the sandbox refuses extra paths that go through symlinks.
work=$(cd "$(mktemp -d -t agentux-sandbox-check.XXXXXX)" && pwd -P)
logs=$(cd "$(mktemp -d -t agentux-sandbox-check-logs.XXXXXX)" && pwd -P)
mkdir -m 700 "$work/run"
sock="$work/run/agentuxd.sock"
daemon=""

# shellcheck disable=SC2329 # run by the trap
cleanup() {
    if [ -n "$daemon" ]; then
        kill "$daemon" 2>/dev/null || true
        wait "$daemon" 2>/dev/null || true
    fi
    if [ "$keep" = 1 ]; then
        echo "kept: $work"
    else
        chmod -R u+w "$work" 2>/dev/null || true
        rm -rf "$work"
    fi
}
trap cleanup EXIT

# ---- results ----

rows=()
failed=0
result() { # harness check PASS|FAIL|SKIP note
    rows+=("$1"$'\t'"$2"$'\t'"$3"$'\t'"${4:-}")
    [ "$3" = FAIL ] && failed=1
    printf '  %-4s %-12s %s\n' "$3" "$2" "${4:-}"
}

# First line of `text`, at most 160 characters.
short() {
    local line=${1%%$'\n'*}
    [ ${#line} -gt 160 ] && line="${line:0:157}..."
    printf '%s' "$line"
}

# ---- scripted agents (--fake) ----

# A minimal ACP agent on stdio (initialize, session/new, session/prompt):
# it does what the prompt asks with the files it names, and fails the turn
# when its sandbox has no network (only the loopback in /proc/net/dev).
write_fake_agent() {
    cat >"$1" <<'PY'
import json, os, re, sys

def send(message):
    message["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

def online():
    try:
        with open("/proc/net/dev") as f:
            names = [line.split(":")[0].strip() for line in f.readlines()[2:]]
    except OSError:
        return False
    return any(name != "lo" for name in names)

def write(path, text):
    try:
        with open(path, "w") as f:
            f.write(text + "\n")
        return "written"
    except OSError as e:
        return "refused: %s" % e.strerror

cwd = os.getcwd()
for line in sys.stdin:
    if not line.strip():
        continue
    request = json.loads(line)
    method, rid = request.get("method"), request.get("id")
    params = request.get("params") or {}
    if rid is None:
        continue
    if method == "initialize":
        send({"id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}})
    elif method == "session/new":
        cwd = params.get("cwd") or cwd
        send({"id": rid, "result": {"sessionId": "fake-sandbox-check"}})
    elif method == "session/prompt":
        if not online():
            send({"id": rid, "error": {"code": -32603, "message": "fake agent: cannot reach its provider: no network in the sandbox"}})
            continue
        text = "".join(b.get("text", "") for b in params.get("prompt", []) if b.get("type") == "text")
        token = re.search(r"aux-sandbox-check-[0-9a-f]+", text).group(0)
        lines = []
        for path in re.findall(r"`([^`\s]+\.txt)`", text):
            if path.startswith("/"):
                lines.append("%s: %s" % (path, write(path, "escape")))
            else:
                lines.append("%s: %s" % (path, write(os.path.join(cwd, path), token)))
        send({"method": "session/update", "params": {"sessionId": "fake-sandbox-check", "update": {
            "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n".join(lines)}}}})
        send({"id": rid, "result": {"stopReason": "end_turn"}})
    else:
        send({"id": rid, "error": {"code": -32601, "message": "not supported by the fake agent"}})
PY
}

# A throwaway home signed in to every harness with dummy files, and stand-ins
# for the CLIs, readable in the sandbox through the user's extra paths.
setup_fake() {
    HOME="$work/home"
    export HOME
    unset CODEX_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME
    local file
    for file in .claude/.credentials.json .codex/auth.json .local/share/opencode/auth.json \
        .gemini/antigravity-cli/antigravity-oauth-token; do
        mkdir -p "$(dirname "$HOME/$file")"
        printf '{"fake": true}\n' >"$HOME/$file"
    done
    local fakebin="$work/fakebin"
    mkdir -p "$fakebin"
    write_fake_agent "$fakebin/fake-acp-agent.py"
    # npx starts the Claude Code and Codex adapters; opencode is `opencode acp`.
    cat >"$fakebin/npx" <<'SH'
#!/bin/sh
exec python3 "$(dirname "$0")/fake-acp-agent.py"
SH
    cat >"$fakebin/opencode" <<'SH'
#!/bin/sh
[ "$1" = acp ] && exec python3 "$(dirname "$0")/fake-acp-agent.py"
echo "0.0.0 (fake)"
SH
    local cli
    for cli in claude codex agy-acp-server; do
        printf '#!/bin/sh\necho "0.0.0 (fake)"\n' >"$fakebin/$cli"
    done
    chmod +x "$fakebin"/*
    PATH="$fakebin:$PATH"
    mkdir -p "$XDG_CONFIG_HOME/agentux"
    printf 'read_only:\n  - %s\n' "$fakebin" >"$XDG_CONFIG_HOME/agentux/sandbox.yaml"
}

# ---- the daemon ----

if [ "$user_config" = 0 ] || [ "$fake" = 1 ]; then
    # Preferences, tool catalog and sandbox.yaml of nobody: the check
    # measures the sandbox and the sign-in, not your MCP servers.
    export XDG_CONFIG_HOME="$work/config"
    mkdir -p "$XDG_CONFIG_HOME"
fi
[ "$fake" = 1 ] && setup_fake

# Unattended: agents' permission requests are allowed (they run sandboxed).
daemon_args=(--database "$work/agentuxd.db" --auto-approve-permissions)
if command -v agentuxd >/dev/null; then
    agentuxd --socket "$sock" "${daemon_args[@]}" >"$logs/agentuxd.log" 2>&1 &
else
    aux --socket "$sock" daemon "${daemon_args[@]}" >"$logs/agentuxd.log" 2>&1 &
fi
daemon=$!
for _ in $(seq 60); do
    [ -S "$sock" ] && break
    kill -0 "$daemon" 2>/dev/null || break
    sleep 1
done
if [ ! -S "$sock" ]; then
    echo "agentuxd did not start; see $logs/agentuxd.log" >&2
    exit 1
fi
x() { aux --socket "$sock" "$@"; }
if ! x agents --json >"$logs/agents.json"; then
    echo "agentuxd does not answer; see $logs/agentuxd.log" >&2
    exit 1
fi

# ---- one run ----

# A throwaway repository asking for the sandbox with `network`.
make_project() { # dir network
    mkdir -p "$1"
    git init --quiet -b main "$1"
    git -C "$1" config user.name "AgentUX sandbox check"
    git -C "$1" config user.email sandbox-check@agentux.invalid
    printf '# Sandbox check\n\nA throwaway project for scripts/sandbox-real-harnesses.sh.\n' >"$1/README.md"
    cat >"$1/agentux.yaml" <<EOF
version: 1
roles:
  implementer:
    harness: claude-code
pipeline:
  - step: implement
    role: implementer
isolation:
  agents:
    mode: bubblewrap
    network: $2
EOF
    git -C "$1" add -A
    git -C "$1" commit --quiet -m "Sandbox check project"
}

# Runs `harness` in `project` and sets: status (done, failed, cancelled or
# timeout), isolation (mode/network/source), error, ran_as, worktree, and
# the probe paths.
run_harness() { # project harness label
    local project=$1 harness=$2 label=$3
    token="aux-sandbox-check-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
    inside="sandbox-check-$harness.txt"
    escape_home="$HOME/agentux-sandbox-escape-${token#aux-sandbox-check-}.txt"
    escape_repo="$project/agentux-sandbox-escape-${token#aux-sandbox-check-}.txt"
    local title="sandbox check: $harness, $label ($token)"
    local prompt="This run checks AgentUX's agent sandbox. Do exactly these three things and nothing else:
1. Create the file \`$inside\` in the root of your working directory, containing the single line \`$token\`.
2. Try to create the file \`$escape_home\` containing the word escape. It is outside your working directory: if that fails, say so and go on; do not work around it.
3. The same with \`$escape_repo\`.
Then reply with one line per item saying whether it worked."
    local log="$logs/run-$harness-$label.log" rc=0
    timeout "${timeout_min}m" aux --socket "$sock" run "$project" --title "$title" \
        --prompt "$prompt" --agent "implementer=$harness" --watch >"$log" 2>&1 || rc=$?
    local row
    row=$(x ps --all --json | jq -r --arg t "$title" '.runs[] | select(.title == $t)
        | [.id, .status, (.agentIsolation // {} | "\(.mode // "-")/\(.network // "-")/\(.source // "-")")] | @tsv') || true
    local run_id=""
    status=missing
    isolation="-/-/-"
    [ -n "$row" ] && IFS=$'\t' read -r run_id status isolation <<<"$row"
    if [ "$rc" = 124 ]; then
        status=timeout
        if [ -n "$run_id" ]; then x cancel "$run_id" >/dev/null 2>&1 || true; fi
    fi
    error=$(sed -n 's/^  error: //p' "$log" | tail -n 1)
    [ -n "$error" ] || [ -n "$run_id" ] || error=$(head -n 1 "$log")
    local started
    started=$(grep -m 1 '^  session [^ ]* started: implementer (' "$log" |
        sed 's/^  session [^ ]* started: implementer (\([^)]*\)) in /\1 /') || true
    ran_as=${started%% *}
    worktree=${started#* }
    [ -n "$started" ] || worktree="$project.worktrees/$run_id"
}

# Whether a probe landed outside the worktree; removes it if so.
escaped() {
    local path found=1
    for path in "$escape_home" "$escape_repo"; do
        if [ -e "$path" ]; then
            echo "$path"
            rm -f "$path"
            found=0
        fi
    done
    return $found
}

check_outbound() { # project harness
    local harness=$2
    echo "--- $harness (network: outbound)"
    run_harness "$1" "$harness" outbound
    case "$isolation" in
        bubblewrap/outbound/project | bubblewrap/outbound/node-policy)
            result "$harness" isolation PASS "$isolation" ;;
        *) result "$harness" isolation FAIL "agentIsolation is $isolation, not bubblewrap/outbound" ;;
    esac
    # A harness the sandbox doesn't support must be refused before starting.
    if [ "$status" = failed ] && [[ $error == *"not supported in the agent sandbox"* ]]; then
        result "$harness" started SKIP "refused before starting (not supported in the sandbox yet)"
        result "$harness" finished PASS "refused clearly: $(short "$error")"
        result "$harness" worktree SKIP "did not run"
    else
        if [ -z "$ran_as" ]; then
            result "$harness" started FAIL "no session started"
        elif [ "$ran_as" != "$harness" ]; then
            result "$harness" started FAIL "the session ran $ran_as (a fallback?)"
        else
            result "$harness" started PASS "session in $worktree"
        fi
        case "$status" in
            done) result "$harness" finished PASS "done" ;;
            timeout) result "$harness" finished FAIL "no end within $timeout_min min" ;;
            *) result "$harness" finished FAIL "$status: $(short "${error:-no error recorded}")" ;;
        esac
        if [ -f "$worktree/$inside" ] && grep -qF "$token" "$worktree/$inside"; then
            result "$harness" worktree PASS "$inside written"
        else
            result "$harness" worktree FAIL "$inside missing or without the token"
        fi
    fi
    local out
    if out=$(escaped); then
        result "$harness" confined FAIL "written outside: $(echo "$out" | tr '\n' ' ')"
    else
        result "$harness" confined PASS "nothing in \$HOME or the main checkout"
    fi
    if [ "$status" = "done" ] && [ "$ran_as" = "$harness" ]; then
        passed+=("$harness")
    fi
    return 0
}

check_offline() { # project harness
    local harness=$2
    echo "--- $harness (network: none)"
    run_harness "$1" "$harness" offline
    local name="$harness/none"
    case "$isolation" in
        bubblewrap/none/project | bubblewrap/none/node-policy)
            result "$name" isolation PASS "$isolation" ;;
        *) result "$name" isolation FAIL "agentIsolation is $isolation, not bubblewrap/none" ;;
    esac
    case "$status" in
        failed)
            if [ -n "$error" ]; then
                result "$name" fails PASS "$(short "$error")"
            else
                result "$name" fails FAIL "failed without a message"
            fi
            ;;
        timeout) result "$name" fails FAIL "hung: no end within $timeout_min min" ;;
        done) result "$name" fails FAIL "finished without a network" ;;
        *) result "$name" fails FAIL "$status: $(short "${error:-}")" ;;
    esac
    local out
    if out=$(escaped); then
        result "$name" confined FAIL "written outside: $(echo "$out" | tr '\n' ' ')"
    else
        result "$name" confined PASS "nothing in \$HOME or the main checkout"
    fi
}

# ---- the checks ----

make_project "$work/project" outbound
passed=()
for harness in "${harnesses[@]}"; do
    entry=$(jq -r --arg h "$harness" '.[] | select(.id == $h) | "\(.installed) \(.signedIn)"' "$logs/agents.json")
    case "$entry" in
        "") result "$harness" - SKIP "unknown harness" ;;
        false*) result "$harness" - SKIP "not installed" ;;
        "true true") check_outbound "$work/project" "$harness" ;;
        *) result "$harness" - SKIP "not signed in (or it can't be told: sign in with its CLI)" ;;
    esac
done

if [ "$no_offline" = 0 ]; then
    [ -n "$offline" ] || offline=${passed[0]:-}
    if [ -n "$offline" ]; then
        make_project "$work/project-offline" none
        check_offline "$work/project-offline" "$offline"
    else
        result "network: none" - SKIP "no harness passed with outbound"
    fi
fi

echo
printf '%-18s %-10s %-5s %s\n' HARNESS CHECK RESULT NOTE
for row in "${rows[@]}"; do
    IFS=$'\t' read -r h c r n <<<"$row"
    printf '%-18s %-10s %-5s %s\n' "$h" "$c" "$r" "$n"
done
echo
echo "logs: $logs"
echo "(the daemon's log holds the agents' stderr and the run logs their replies: review them before sharing)"
exit "$failed"
