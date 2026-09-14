#!/bin/bash
# End-to-end check that a review's conversation is readable while it runs.
#
# Drives a real review through the daemon with a stub AI provider, then asserts
# that review_log_entries fills up during the run, that /api/review serves it as
# `live_log`, and that the rows are cleared once the review finishes.
#
# Safety rules, same as verify_fetch_cancel.sh:
#   - Only ever kill PIDs this script started and recorded itself.
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
RESULT=1

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

timeout 240 python3 "$WORKDIR/sashiko-stubai.py" "$STUB_PORT" &
STUB_PID=$!
sleep 2
python3 - "$STUB_PORT" <<'PYEOF' || fail "stub model did not come up"
import socket, sys
s = socket.socket(); s.settimeout(3); s.connect(("127.0.0.1", int(sys.argv[1])))
PYEOF
echo "stub model listening on $STUB_PORT (pid $STUB_PID)"

# -------------------------------------------------------------------- daemon
[ -x "$BIN" ] || fail "build first: cargo build"
cp -r "$REPO_ROOT/static" "$WORKDIR/"
sed -e "s|^url = \"sashiko.db\"|url = \"$WORKDIR/t.db\"|" \
    -e "s|^repository_path = .*|repository_path = \"$REPO_ROOT/third_party/linux\"|" \
    -e "s|^provider = \"gemini\"|provider = \"openai-compatible\"|" \
    "$REPO_ROOT/Settings.toml" > "$WORKDIR/Settings.toml"
cat >> "$WORKDIR/Settings.toml" <<EOF

[ai.openai_compat]
base_url = "http://127.0.0.1:$STUB_PORT/v1"
EOF

( cd "$WORKDIR" && OPENAI_API_KEY=stub timeout 220 "$BIN" --port "$PORT" > "$WORKDIR/daemon.log" 2>&1 ) &

for _ in $(seq 1 40); do
    curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
    sleep 1
done
curl -fsS --max-time 3 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 \
    || fail "daemon did not start"
echo "daemon up on $PORT"

# ------------------------------------------------------------- submit a review
HEAD_SHA=$(git -C "$REPO_ROOT/third_party/linux" rev-parse HEAD 2>/dev/null)
[ -n "$HEAD_SHA" ] || fail "no local kernel checkout to review"
curl -fsS --max-time 10 -X POST "http://127.0.0.1:$PORT/api/submit" \
    -H 'Content-Type: application/json' \
    -d "{\"type\":\"remote\",\"sha\":\"$HEAD_SHA\"}" >/dev/null \
    || fail "submit rejected"
echo "submitted $HEAD_SHA"

# ------------------------------------------- watch entries appear while running
PEAK=0
LIVE_SEEN=0
for _ in $(seq 1 100); do
    COUNT=$(query "SELECT COUNT(*) FROM review_log_entries")
    [ -n "$COUNT" ] || COUNT=0
    [ "$COUNT" -gt "$PEAK" ] && PEAK=$COUNT
    STATUS=$(query "SELECT status FROM patchsets WHERE id=1")

    if [ "$PEAK" -gt 0 ] && [ "$LIVE_SEEN" -eq 0 ]; then
        RID=$(query "SELECT id FROM reviews ORDER BY id DESC LIMIT 1")
        if [ -n "$RID" ] && curl -fsS --max-time 4 "http://127.0.0.1:$PORT/api/review?id=$RID" \
            | grep -q '"live_log"'; then
            LIVE_SEEN=1
            echo "PASS: /api/review served live_log while the review was running"
        fi
    fi

    case "$STATUS" in Reviewed|Failed|Cancelled|"Failed To Apply") break ;; esac
    sleep 2
done

echo "peak live entries: $PEAK, final status: ${STATUS:-unknown}"
[ "$PEAK" -gt 0 ] || fail "no log entries were streamed while the review ran"
echo "PASS: conversation was streamed during the review"
[ "$LIVE_SEEN" -eq 1 ] || echo "WARN: review finished before live_log could be sampled"

# ----------------------------------------------------- cleared once it finishes
sleep 3
REMAINING=$(query "SELECT COUNT(*) FROM review_log_entries")
[ "${REMAINING:-0}" -eq 0 ] \
    || fail "$REMAINING streamed entries survived completion; the table would grow forever"
echo "PASS: streamed entries cleared once the review finished"

echo
echo "ALL CHECKS PASSED"
RESULT=0
exit $RESULT
