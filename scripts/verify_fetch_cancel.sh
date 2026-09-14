#!/bin/bash
# End-to-end check that cancelling a patchset stops an in-flight git fetch.
#
# A fetch only hangs if the far end accepts the connection and never replies, so
# this stands up a local socket that does exactly that, points Sashiko at it, and
# cancels mid-fetch.
#
# Safety rules, learned the hard way:
#   - Only ever kill PIDs this script started and recorded itself.
#   - Never pass a negative PID to kill. A negative PID means "this whole process
#     group", and `-0` means the caller's own group, which takes out the calling
#     shell and can end a WSL session.
#   - Never use `pkill -f`. The pattern matches this script's own command line,
#     so it kills its own shell.
#   - Verify a PID still looks like what we started before signalling it, in case
#     the process died and Linux recycled the number.

set -u

PORT=18777
HANG_PORT=19777
WORKDIR=$(mktemp -d /tmp/sashiko-fetchcancel.XXXXXX)
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO_ROOT/target/debug/sashiko"

HANG_PID=""
DAEMON_PID=""
RESULT=1

# Kills one PID, but only after confirming it is still the process we started.
safe_kill() {
    local pid="$1" marker="$2"
    [ -n "$pid" ] || return 0
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac   # digits only: never negative
    [ "$pid" -gt 1 ] || return 0                   # never init, never group 0
    [ -r "/proc/$pid/cmdline" ] || return 0
    if tr '\0' ' ' < "/proc/$pid/cmdline" | grep -q -- "$marker"; then
        kill "$pid" 2>/dev/null || true
    fi
}

cleanup() {
    safe_kill "$DAEMON_PID" "sashiko"
    safe_kill "$HANG_PID" "sashiko-hangserver"
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

fail() { echo "FAIL: $*"; exit 1; }

# ---------------------------------------------------------------- hang server
cat > "$WORKDIR/sashiko-hangserver.py" <<'PYEOF'
# Accepts TCP connections and never replies, so `git fetch` blocks on read.
import socket, sys, time
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", int(sys.argv[1])))
s.listen(16)
held = []
deadline = time.time() + 300
while time.time() < deadline:
    s.settimeout(5)
    try:
        c, _ = s.accept()
        held.append(c)          # keep it open, send nothing
    except socket.timeout:
        pass
PYEOF

python3 "$WORKDIR/sashiko-hangserver.py" "$HANG_PORT" &
HANG_PID=$!
sleep 2
python3 - "$HANG_PORT" <<'PYEOF' || fail "hang server did not come up"
import socket, sys
s = socket.socket(); s.settimeout(3)
s.connect(("127.0.0.1", int(sys.argv[1])))
PYEOF
echo "hang server listening on $HANG_PORT (pid $HANG_PID)"

# -------------------------------------------------------------------- daemon
[ -x "$BIN" ] || fail "build first: cargo build"
cp -r "$REPO_ROOT/static" "$WORKDIR/"
sed -e "s|^url = \"sashiko.db\"|url = \"$WORKDIR/t.db\"|" \
    -e "s|^repository_path = .*|repository_path = \"$REPO_ROOT/third_party/linux\"|" \
    "$REPO_ROOT/Settings.toml" > "$WORKDIR/Settings.toml"

# `timeout` is the real safety net here: $! is the subshell, not the daemon, so
# the trap below may decline to signal it. The daemon exits on its own either
# way, which is preferable to guessing at PIDs.
( cd "$WORKDIR" && GEMINI_API_KEY=dummy timeout 180 "$BIN" --port "$PORT" > "$WORKDIR/daemon.log" 2>&1 ) &
DAEMON_PID=$!

for _ in $(seq 1 40); do
    if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then break; fi
    sleep 1
done
curl -fsS --max-time 3 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 \
    || fail "daemon did not start; see $WORKDIR/daemon.log"
echo "daemon up on $PORT"

# --------------------------------------------------------------- submit fetch
curl -fsS --max-time 8 -X POST "http://127.0.0.1:$PORT/api/submit" \
    -H 'Content-Type: application/json' \
    -d "{\"type\":\"remote\",\"sha\":\"cafe12345678\",\"repo\":\"http://127.0.0.1:$HANG_PORT/hang.git\"}" \
    >/dev/null || fail "submit rejected"

# The queue ticks every 10s, so give it time to actually start fetching.
GIT_PID=""
for _ in $(seq 1 30); do
    GIT_PID=$(ps -eo pid,args --no-headers \
        | grep 'fetch fetcher-' | grep -v grep | awk '{print $1}' | head -1)
    [ -n "$GIT_PID" ] && break
    sleep 1
done
[ -n "$GIT_PID" ] || fail "no git fetch started; see $WORKDIR/daemon.log"
echo "git fetch hung as expected (pid $GIT_PID)"

# --------------------------------------------------------------------- cancel
CANCEL=$(curl -fsS --max-time 8 -X POST \
    "http://127.0.0.1:$PORT/api/patchset/cancel?id=1&force=true")
echo "cancel response: $CANCEL"
echo "$CANCEL" | grep -q '"status":"cancelled"' || fail "patchset was not cancelled"
echo "$CANCEL" | grep -q '"interrupted":true' || fail "no live fetch was interrupted"

sleep 4

# ---------------------------------------------------------------- assertions
if [ -r "/proc/$GIT_PID/cmdline" ]; then
    fail "git fetch $GIT_PID survived the cancel"
fi
echo "PASS: git fetch process was killed"

grep -q 'not falling back' "$WORKDIR/daemon.log" \
    || fail "expected the full-fetch fallback to be skipped after cancellation"
echo "PASS: no full-fetch fallback after cancellation"

STATUS=$(python3 - "$WORKDIR/t.db" <<'PYEOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print(next(c.execute("SELECT status FROM patchsets WHERE id=1"))[0])
PYEOF
)
echo "patchset status: $STATUS"
[ "$STATUS" = "Cancelled" ] || [ "$STATUS" = "Failed" ] \
    || fail "unexpected terminal status: $STATUS"
echo "PASS: patchset reached a terminal state"

# Helper processes are knowingly left to the kernel's socket timeout; see the
# limitation noted on FetchAgent::fetch_with_cancel.
LEFTOVER=$(ps -eo args --no-headers | grep -c "git-remote-http.*$HANG_PORT" || true)
echo "note: $LEFTOVER git helper process(es) still winding down (expected, harmless)"

echo
echo "ALL CHECKS PASSED"
RESULT=0
exit $RESULT
