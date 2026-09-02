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
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DELAY = float(sys.argv[2])

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        # Stalling keeps the review in flight long enough to be asked about.
        time.sleep(DELAY)
        body = json.dumps({
            "id": "stub", "object": "chat.completion", "model": "stub",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant",
                "content": json.dumps({"concerns": [], "dismissed_concerns": []}),
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

    ( cd "$DIR" && OPENAI_API_KEY=stub timeout 300 "$BIN" --port "$port" > "$DIR/daemon.log" 2>&1 ) &

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
check "the patch's own phase gets a row too" 'grep -q "this patch" "$ROOT/running-timings.txt"'
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
check "the recorded breakdown is shown" 'grep -qE "^    [0-9]+ +[0-9]" "$ROOT/done-timings.txt"'
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
