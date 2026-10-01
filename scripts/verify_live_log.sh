#!/bin/bash
# End-to-end check that a review's conversation is readable while it runs.
#
# Drives a real review through the daemon with a stub AI provider, then asserts
# that review_log_entries fills up during the run with stage names stored as
# text, that /api/review serves it as `live_log` in the finished log's message
# shape, that the public activity route never carries any of it, and that the
# rows are cleared once the review finishes.
#
# Needs a debug build (cargo build), python3, curl and the kernel checkout at
# third_party/linux to review its HEAD.
#
# Safety rules:
#   - Only ever kill PIDs this script started and recorded itself, and only
#     after checking the process's command line still carries our marker.
#   - Never pass a negative PID to kill; a negative PID means a whole process
#     group, and `-0` is the caller's own group.
#   - Never use `pkill -f`; the pattern matches this script's own command line.
#   - `timeout` on every background process is the real backstop.

set -u

PORT=18778
STUB_PORT=19778
WORKDIR=$(mktemp -d /tmp/sashiko-livelog.XXXXXX)
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO_ROOT/target/debug/sashiko"

STUB_PID=""
DAEMON_PID=""

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
    safe_kill "$DAEMON_PID" "--port $PORT"
    safe_kill "$STUB_PID" "sashiko-stubai"
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

fail() { echo "FAIL: $*"; [ -f "$WORKDIR/daemon.log" ] && tail -20 "$WORKDIR/daemon.log"; exit 1; }

query() {
    python3 - "$WORKDIR/t.db" "$1" <<'PYEOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
try:
    print(next(c.execute(sys.argv[2]))[0])
except StopIteration:
    print("")
PYEOF
}

# ------------------------------------------------------- stub OpenAI-ish model
# Answers slowly so the review stays in flight long enough to observe, and
# eventually returns the JSON shape a stage expects.
cat > "$WORKDIR/sashiko-stubai.py" <<'PYEOF'
import json, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        self.rfile.read(n)
        time.sleep(3)                      # keep the review visibly running
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

HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PYEOF

timeout 300 python3 "$WORKDIR/sashiko-stubai.py" "$STUB_PORT" &
STUB_PID=$!
sleep 2
python3 - "$STUB_PORT" <<'PYEOF' || fail "stub model did not come up"
import socket, sys
s = socket.socket(); s.settimeout(3); s.connect(("127.0.0.1", int(sys.argv[1])))
PYEOF
echo "stub model listening on $STUB_PORT (pid $STUB_PID)"

# -------------------------------------------------------------------- daemon
[ -x "$BIN" ] || fail "build first: cargo build"
HEAD_SHA=$(git -C "$REPO_ROOT/third_party/linux" rev-parse HEAD 2>/dev/null)
[ -n "$HEAD_SHA" ] || fail "no local kernel checkout to review"

# The web UI's static files are read relative to the working directory.
cp -r "$REPO_ROOT/static" "$WORKDIR/"
sed -e "s|^url = \"sashiko.db\"|url = \"$WORKDIR/t.db\"|" \
    -e "s|^repository_path = .*|repository_path = \"$REPO_ROOT/third_party/linux\"|" \
    -e "s|^provider = \"gemini\"|provider = \"openai-compatible\"|" \
    "$REPO_ROOT/Settings.toml" > "$WORKDIR/Settings.toml"
cat >> "$WORKDIR/Settings.toml" <<EOF

[ai.openai_compat]
base_url = "http://127.0.0.1:$STUB_PORT/v1"
EOF

# exec, so the recorded PID is the timeout wrapper itself, which passes a
# SIGTERM on to the daemon.
( cd "$WORKDIR" && OPENAI_API_KEY=stub exec timeout 280 "$BIN" --port "$PORT" \
    > "$WORKDIR/daemon.log" 2>&1 ) &
DAEMON_PID=$!

for _ in $(seq 1 60); do
    curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
    sleep 1
done
curl -fsS --max-time 3 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 \
    || fail "daemon did not start"
echo "daemon up on $PORT (pid $DAEMON_PID)"

# Upstream gates submission on the local operator token rather than on the
# caller being loopback. The daemon writes it beside its database.
TOKEN_FILE="$WORKDIR/.sashiko-local-token"
for _ in $(seq 1 10); do [ -s "$TOKEN_FILE" ] && break; sleep 1; done
[ -s "$TOKEN_FILE" ] || fail "no local operator token at $TOKEN_FILE"

# ------------------------------------------------------------- submit a review
curl -fsS --max-time 10 -X POST "http://127.0.0.1:$PORT/api/submit" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $(cat "$TOKEN_FILE")" \
    -d "{\"type\":\"remote\",\"sha\":\"$HEAD_SHA\"}" >/dev/null \
    || fail "submit rejected"
echo "submitted $HEAD_SHA"

# Checks one /api/review body and the patchset's activity body taken beside
# it: live_log entries are AiMessages with their position, and none of their
# text reaches the public activity route.
check_live() {
    python3 - "$1" "$2" <<'PYEOF'
import json, sys
review = json.load(open(sys.argv[1]))
activity = open(sys.argv[2]).read()
live = review.get("live_log")
if not isinstance(live, list) or not live:
    sys.exit(2)                                   # nothing to check yet
for e in live:
    if e.get("role") not in ("system", "user", "assistant", "tool"):
        sys.exit(f"entry without an AiMessage role: {e}")
    if not (isinstance(e.get("content"), str) or isinstance(e.get("tool_calls"), list)):
        sys.exit(f"entry with neither content nor tool_calls: {e}")
    if not isinstance(e.get("seq"), int):
        sys.exit(f"entry without a numeric seq: {e}")
    if e.get("stage") is not None and not isinstance(e["stage"], str):
        sys.exit(f"stage is not a name: {e}")
# The run marker the page uses to notice a restarted run: the lowest stored seq.
if review.get("live_log_first_seq") != min(e["seq"] for e in live):
    sys.exit(f"live_log_first_seq {review.get('live_log_first_seq')!r} is not the first seq")
if "live_log" in activity:
    sys.exit("the activity route carries a live_log")
for e in live:
    text = (e.get("content") or "").strip()
    # Long enough to be distinctive; activity descriptions are short phrases.
    for line in text.splitlines():
        line = line.strip()
        if len(line) >= 40 and line[:40] in activity:
            sys.exit(f"conversation text reached the activity route: {line[:40]!r}")
print(len(live))
PYEOF
}

# ------------------------------------------- watch entries appear while running
PEAK=0
LIVE_SEEN=0
STATUS=""
for _ in $(seq 1 120); do
    COUNT=$(query "SELECT COUNT(*) FROM review_log_entries")
    [ -n "$COUNT" ] || COUNT=0
    [ "$COUNT" -gt "$PEAK" ] && PEAK=$COUNT
    STATUS=$(query "SELECT status FROM patchsets ORDER BY id DESC LIMIT 1")

    if [ "$COUNT" -gt 0 ]; then
        KINDS=$(query "SELECT group_concat(DISTINCT typeof(stage)) FROM review_log_entries WHERE stage IS NOT NULL")
        case "$KINDS" in ''|text) ;; *) fail "stage stored as $KINDS, not text" ;; esac
    fi

    if [ "$COUNT" -gt 0 ] && [ "$LIVE_SEEN" -eq 0 ]; then
        RID=$(query "SELECT review_id FROM review_log_entries ORDER BY id DESC LIMIT 1")
        PSID=$(query "SELECT patchset_id FROM reviews WHERE id = $RID")
        if [ -n "$RID" ] && [ -n "$PSID" ] \
            && curl -fsS --max-time 4 "http://127.0.0.1:$PORT/api/review?id=$RID" > "$WORKDIR/review.json" \
            && curl -fsS --max-time 4 "http://127.0.0.1:$PORT/api/patchset/activity?id=$PSID" > "$WORKDIR/activity.json"; then
            N=$(check_live "$WORKDIR/review.json" "$WORKDIR/activity.json" 2>&1)
            RC=$?
            if [ "$RC" -eq 0 ]; then
                LIVE_SEEN=1
                echo "PASS: /api/review served $N live_log entries shaped as messages, none in the activity route"
            elif [ "$RC" -ne 2 ]; then
                fail "live_log check: $N"
            fi
        fi
    fi

    case "$STATUS" in Reviewed|Failed|Cancelled|"Failed To Apply"|Skipped) break ;; esac
    sleep 2
done

echo "peak live entries: $PEAK, final status: ${STATUS:-unknown}"
[ "$PEAK" -gt 0 ] || fail "no log entries were streamed while the review ran"
echo "PASS: conversation was streamed during the review, stages stored as text"
[ "$LIVE_SEEN" -eq 1 ] || fail "review finished before live_log could be sampled"

# ----------------------------------------------------- cleared once it finishes
sleep 3
REMAINING=$(query "SELECT COUNT(*) FROM review_log_entries")
[ "${REMAINING:-0}" -eq 0 ] \
    || fail "$REMAINING streamed entries survived completion; the table would grow forever"
echo "PASS: streamed entries cleared once the review finished"

RID=$(query "SELECT id FROM reviews ORDER BY id DESC LIMIT 1")
curl -fsS --max-time 4 "http://127.0.0.1:$PORT/api/review?id=$RID" | python3 -c '
import json, sys
body = json.load(sys.stdin)
sys.exit("a finished review still serves live_log" if "live_log" in body else 0)
' || fail "live_log outlived the review"
echo "PASS: a finished review serves no live_log"

echo
echo "ALL CHECKS PASSED"
exit 0
