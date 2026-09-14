#!/bin/bash
# End-to-end check for `sashiko-cli show <id> --timings`.
#
# The flag used to print nothing at all: the per-patch section it lived in sat
# behind a review-log fetch that only happens for terminal statuses, so asking
# for timings while a review was running -- the case worth asking about --
# produced output identical to not asking.
#
# Two cases, both against a real daemon and a stub model:
#   running   the stub stalls, so the patchset is still In Review when asked
#   finished  the stub answers immediately
#
# Uses a throwaway git repo rather than a kernel checkout, so it needs nothing
# but a build.
#
# Safety rules, as in the other verify scripts:
#   - Only ever kill PIDs this script started and recorded itself.
#   - Never pass a negative PID to kill; a negative PID means a whole process
#     group, and `-0` is the caller's own group.
#   - Never use `pkill -f`; the pattern matches this script's own command line.
#   - `timeout` on every background process is the real backstop.

set -u

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO_ROOT/target/debug/sashiko"
CLI="$REPO_ROOT/target/debug/sashiko-cli"
ROOT=$(mktemp -d /tmp/sashiko-timings.XXXXXX)

SLOW_PID=""
FAST_PID=""
# Daemons this script started, so a run does not leave one holding its port.
# `timeout 300` eventually reaps them, but that is long enough for a second run
# to collide and fail with something that looks nothing like the real cause.
DAEMON_PIDS=""

safe_kill() {
    local pid="$1" marker="$2"
    [ -n "$pid" ] || return 0
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac
    [ "$pid" -gt 1 ] || return 0
    [ -r "/proc/$pid/cmdline" ] || return 0
    if tr '\0' ' ' < "/proc/$pid/cmdline" | grep -q -- "$marker"; then
        kill "$pid" 2>/dev/null || true
    fi
}

cleanup() {
    safe_kill "$SLOW_PID" "sashiko-stubai"
    safe_kill "$FAST_PID" "sashiko-stubai"
    local pid
    for pid in $DAEMON_PIDS; do
        # The recorded pid is the `timeout` wrapping the daemon, whose command
        # line carries the binary path, so safe_kill's marker check applies to
        # it as it does to the stubs. `timeout` forwards the signal to the
        # daemon, so there is no need to hunt for the child -- which is just as
        # well, since the rules above rule out `pkill -f`.
        safe_kill "$pid" "$BIN"
    done
    rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*"; exit 1; }
failures=0
check() {
    local name="$1" expr="$2"
    if eval "$expr" >/dev/null 2>&1; then
        echo "  ok   $name"
    else
        failures=$((failures + 1))
        echo "  FAIL $name"
    fi
}

[ -x "$BIN" ] && [ -x "$CLI" ] || fail "build first: cargo build"

# ------------------------------------------------------------- test repository
SRC="$ROOT/src"
mkdir -p "$SRC"
git -C "$SRC" init -q
git -C "$SRC" config user.email t@example.com
git -C "$SRC" config user.name Tester
printf 'int base(void) { return 0; }\n' > "$SRC/a.c"
git -C "$SRC" add a.c
git -C "$SRC" commit -qm "base: add a.c"
BASE=$(git -C "$SRC" rev-parse HEAD)
printf 'int base(void) { return 0; }\nint added(void) { return 1; }\n' > "$SRC/a.c"
git -C "$SRC" commit -qam "a: add added()"
HEAD_SHA=$(git -C "$SRC" rev-parse HEAD)

# ------------------------------------------------------------------ stub model
cat > "$ROOT/sashiko-stubai.py" <<'PYEOF'
# Answers each review stage with the shape that stage asks for.
#
# A single canned reply is not enough: the stages validate their output against
# different schemas, and one that fails validation is retried three times and
# then fails the whole review. Answering only the analysis shape meant every
# review died in the planning stage, before a single stage had started -- so
# this script asserted against a review that never ran.
#
# Each stage's prompt names the key it wants back, so the request says which
# stage is asking.
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DELAY = float(sys.argv[2])

# Carried through deduplication and conflict resolution, so the pipeline runs
# its full length instead of exiting early for want of anything to consolidate.
CONCERN = {
    "description": "stub concern",
    "severity": "Low",
    "locations": [],
}


def reply_for(prompt):
    # Most specific first: several prompts mention "concerns" in passing.
    if "relevant_stages" in prompt:
        # Restricted by schema to the optional analysis stages. Asking for all
        # of them widens the concurrent fan-out, which is the thing the timings
        # table exists to show.
        return {"relevant_stages": ["resources", "locking", "security", "hardware"]}
    if "selected_prompts" in prompt:
        return {"selected_prompts": []}
    if "'findings' array" in prompt:
        # Deliberately empty. The report stage that would follow validates a
        # strict inline format -- commit and author headers, quoted context --
        # that a stub has no way to produce, and the workflow exits before it
        # when verification validates nothing.
        return {"findings": []}
    # Analysis, deduplication and conflict resolution all accept this shape.
    return {"concerns": [CONCERN], "dismissed_concerns": []}


class H(BaseHTTPRequestHandler):
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", 0)))
        try:
            prompt = " ".join(
                str(m.get("content", ""))
                for m in json.loads(raw).get("messages", [])
            )
        except (ValueError, AttributeError):
            prompt = ""
        # Stalling keeps the review in flight long enough to be asked about.
        time.sleep(DELAY)
        body = json.dumps({
            "id": "stub", "object": "chat.completion", "model": "stub",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant",
                "content": json.dumps(reply_for(prompt)),
            }}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a):
        pass

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PYEOF

SLOW_PORT=19881
FAST_PORT=19882
timeout 420 python3 "$ROOT/sashiko-stubai.py" "$SLOW_PORT" 5 &
SLOW_PID=$!
timeout 420 python3 "$ROOT/sashiko-stubai.py" "$FAST_PORT" 0 &
FAST_PID=$!
sleep 2

# Starts a daemon against one stub and submits the range.
# $1 = label, $2 = daemon port, $3 = stub port.
start_case() {
    local label="$1" port="$2" stub="$3"
    DIR="$ROOT/$label"
    mkdir -p "$DIR"
    cp -r "$REPO_ROOT/static" "$DIR/"
    sed -e "s|^url = \"sashiko.db\"|url = \"$DIR/t.db\"|" \
        -e "s|^repository_path = .*|repository_path = \"$SRC\"|" \
        -e "s|^provider = \"gemini\"|provider = \"openai-compatible\"|" \
        "$REPO_ROOT/Settings.toml" > "$DIR/Settings.toml"
    cat >> "$DIR/Settings.toml" <<EOF

[ai.openai_compat]
base_url = "http://127.0.0.1:$stub/v1"
EOF

    # `exec` so the recorded pid is the `timeout` itself rather than a subshell
    # wrapping it. A subshell's command line is this script's, which safe_kill
    # would rightly refuse to match -- leaving the daemon running and the next
    # run failing on a port that is still held.
    ( cd "$DIR" && export OPENAI_API_KEY=stub \
        && exec timeout 300 "$BIN" --port "$port" > "$DIR/daemon.log" 2>&1 ) &
    DAEMON_PIDS="$DAEMON_PIDS $!"

    local i
    for i in $(seq 1 40); do
        curl -fsS --max-time 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1 && break
        sleep 1
    done
    curl -fsS --max-time 3 "http://127.0.0.1:$port/health" >/dev/null 2>&1 \
        || fail "[$label] daemon did not start"

    SERVER="http://127.0.0.1:$port"
    "$CLI" --server "$SERVER" submit --type range "$BASE..$HEAD_SHA" -r "$SRC" >/dev/null \
        || fail "[$label] submit rejected"
}

# Waits until `show` reports one of the given statuses. $1 = extglob pattern.
wait_for_status() {
    local want="$1" i st
    for i in $(seq 1 90); do
        st=$("$CLI" --server "$SERVER" show 1 2>/dev/null \
             | sed -n 's/^  Status:  *//p' | head -1)
        case "$st" in $want) STATUS="$st"; return 0 ;; esac
        sleep 2
    done
    STATUS="${st:-unknown}"
    return 1
}

# Reaching In Review only means the patches were dispatched. The worker is a
# subprocess and takes a moment to emit its first stage event, so waiting on the
# status alone races it.
wait_for_stage_activity() {
    local i
    for i in $(seq 1 90); do
        if curl -fsS --max-time 3 "$SERVER/api/patchset/activity?id=1" 2>/dev/null \
           | grep -q '"key":"patchset:1/patch:[0-9]*/stage:'; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# ------------------------------------------------- case 1: while it is running
start_case running 18881 "$SLOW_PORT"
wait_for_status "In Review" || fail "[running] never reached In Review (got '$STATUS')"
wait_for_stage_activity || fail "[running] no stage activity ever appeared"

"$CLI" --server "$SERVER" show 1 > "$ROOT/running-plain.txt" 2>&1
"$CLI" --server "$SERVER" show 1 --timings > "$ROOT/running-timings.txt" 2>&1
curl -fsS --max-time 3 "$SERVER/api/patchset/activity?id=1" > "$ROOT/activity.json" 2>&1 || true

echo
echo "case 1: while the review is running"
check "--timings changes the output" '! diff -q "$ROOT/running-plain.txt" "$ROOT/running-timings.txt" >/dev/null'
check "a stage timings section appears" 'grep -q "Stage timings:" "$ROOT/running-timings.txt"'
check "the table is per patch" 'grep -q "Patch 1:" "$ROOT/running-timings.txt"'
check "columns are labelled" 'grep -qE "STAGE .*ELAPSED .*TURNS .*STATE" "$ROOT/running-timings.txt"'
check "a live stage is reported" 'grep -qE "awaiting model|queued for a model slot|running " "$ROOT/running-timings.txt"'
# Names, not numbers, and not the patch's own label: reading the stage as an
# integer made every row fall back to "this patch", so a table of concurrent
# stages read as a column of identical rows and still passed every check above.
check "live rows name their stage" 'grep -qE "^    [a-z][a-z-]+ +[0-9]+s +turn [0-9]+/" "$ROOT/running-timings.txt"'
check "the patch's own phase gets a row too" 'grep -q "this patch" "$ROOT/running-timings.txt"'
check "only one row is the patch's own" '[ "$(grep -c "this patch" "$ROOT/running-timings.txt")" -eq 1 ]'
check "a running stage shows its turn counter" 'grep -qE "turn [0-9]+/[0-9]+" "$ROOT/running-timings.txt"'
check "plain output still says nothing about stages" '! grep -q "Stage timings:" "$ROOT/running-plain.txt"'

# --------------------------------------------------- case 2: once it has ended
start_case finished 18882 "$FAST_PORT"
wait_for_status "Reviewed" || fail "[finished] never reached Reviewed (got '$STATUS')"

"$CLI" --server "$SERVER" show 1 > "$ROOT/done-plain.txt" 2>&1
"$CLI" --server "$SERVER" show 1 --timings > "$ROOT/done-timings.txt" 2>&1

echo
echo "case 2: once the review has finished"
check "--timings changes the output" '! diff -q "$ROOT/done-plain.txt" "$ROOT/done-timings.txt" >/dev/null'
check "a stage timings section appears" 'grep -q "Stage timings:" "$ROOT/done-timings.txt"'
# Stages are named, not numbered. This pattern is what catches a renderer that
# reads the name as an integer: the row still appears, but the stage column is a
# "0" that says nothing about which stage ran.
check "the recorded breakdown names its stages" 'grep -qE "^    [a-z][a-z-]+ +[0-9]+s" "$ROOT/done-timings.txt"'
check "the overlap is shown rather than asserted" 'grep -qE "stages overlapping: longest .*, .* summed" "$ROOT/done-timings.txt"'
check "the note carries this review's numbers, not boilerplate" '! grep -q "stages run concurrently" "$ROOT/done-timings.txt"'

echo
if [ "$failures" -ne 0 ]; then
    echo "--- activity endpoint (running) ---"; cat "$ROOT/activity.json"; echo
    echo "--- with --timings (running) ---"; cat "$ROOT/running-timings.txt"
    echo "--- with --timings (finished) ---"; cat "$ROOT/done-timings.txt"
    echo "$failures FAILURE(S)"
    exit 1
fi
echo "ALL CHECKS PASSED"
