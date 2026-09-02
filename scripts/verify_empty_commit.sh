#!/bin/bash
# End-to-end check for empty commits in a submitted range.
#
# Uses the b4 series that prompted this: an empty `b4 prep` tracker commit
# followed by one real patch. Asserts the tracker is never reviewed and never
# retried, both with and without --b4-cover-letter.
#
# Each case gets its own database and daemon. Resubmitting the same range to one
# database reuses the existing patchset, which would silently skip the second
# case entirely.
#
# Safety rules, as in the other verify scripts:
#   - Only ever kill PIDs this script started and recorded itself.
#   - Never pass a negative PID to kill; a negative PID means a whole process
#     group, and `-0` is the caller's own group.
#   - Never use `pkill -f`; the pattern matches this script's own command line.
#   - `timeout` on every background process is the real backstop.

set -u

KERNEL=${KERNEL:-$HOME/git/net-next}
BRANCH=${BRANCH:-b4/ionic-hwstamp-free-on-disable}
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO_ROOT/target/debug/sashiko"
CLI="$REPO_ROOT/target/debug/sashiko-cli"
ROOT=$(mktemp -d /tmp/sashiko-emptycommit.XXXXXX)

STUB_PID=""
STUB_PORT=19779

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
    rm -rf "$ROOT"
}
trap cleanup EXIT

fail() { echo "FAIL: $*"; exit 1; }

query() {
    python3 - "$1" "$2" <<'PYEOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
try:
    print(next(c.execute(sys.argv[2]))[0])
except StopIteration:
    print("")
PYEOF
}

[ -x "$BIN" ] && [ -x "$CLI" ] || fail "build first: cargo build"
[ -d "$KERNEL/.git" ] || fail "no kernel checkout at $KERNEL"
HEAD_SHA=$(git -C "$KERNEL" rev-parse "$BRANCH" 2>/dev/null) || fail "no branch $BRANCH"
BASE_SHA=$(git -C "$KERNEL" rev-parse "$BRANCH~2" 2>/dev/null) || fail "branch too short"
TRACKER=$(git -C "$KERNEL" rev-parse "$BRANCH~1")

# The premise: the first commit really does change nothing.
CHANGED=$(git -C "$KERNEL" diff-tree --no-commit-id --name-only -r --root "$TRACKER" | wc -l)
[ "$CHANGED" -eq 0 ] || fail "expected $TRACKER to be empty, it changes $CHANGED file(s)"
echo "tracker $TRACKER changes no files, as expected"

# --------------------------------------------------------------- stub model
cat > "$ROOT/sashiko-stubai.py" <<'PYEOF'
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
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

timeout 420 python3 "$ROOT/sashiko-stubai.py" "$STUB_PORT" &
STUB_PID=$!
sleep 2

# Runs one case in its own database. $1 = label, $2 = port, $3... = extra CLI args.
run_case() {
    local label="$1" port="$2"; shift 2
    local dir="$ROOT/$label"
    mkdir -p "$dir"
    cp -r "$REPO_ROOT/static" "$dir/"
    sed -e "s|^url = \"sashiko.db\"|url = \"$dir/t.db\"|" \
        -e "s|^repository_path = .*|repository_path = \"$KERNEL\"|" \
        -e "s|^provider = \"gemini\"|provider = \"openai-compatible\"|" \
        "$REPO_ROOT/Settings.toml" > "$dir/Settings.toml"
    cat >> "$dir/Settings.toml" <<EOF

[ai.openai_compat]
base_url = "http://127.0.0.1:$STUB_PORT/v1"
EOF

    ( cd "$dir" && OPENAI_API_KEY=stub timeout 200 "$BIN" --port "$port" > "$dir/daemon.log" 2>&1 ) &

    local i
    for i in $(seq 1 40); do
        curl -fsS --max-time 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1 && break
        sleep 1
    done
    curl -fsS --max-time 3 "http://127.0.0.1:$port/health" >/dev/null 2>&1 \
        || fail "[$label] daemon did not start"

    "$CLI" --server "http://127.0.0.1:$port" submit --type range "$@" \
        "$BASE_SHA..$HEAD_SHA" -r "$KERNEL" >/dev/null || fail "[$label] submit rejected"

    local st
    for i in $(seq 1 80); do
        st=$(query "$dir/t.db" "SELECT status FROM patchsets ORDER BY id DESC LIMIT 1")
        case "$st" in Reviewed|Failed|Cancelled|"Failed To Apply") break ;; esac
        sleep 2
    done

    CASE_DIR="$dir"
    CASE_STATUS="$st"
    CASE_PATCHES=$(query "$dir/t.db" "SELECT COUNT(*) FROM patches")
    CASE_SKIPPED=$(query "$dir/t.db" "SELECT COUNT(*) FROM patches WHERE status = 'Skipped'")
    CASE_APPLY_FAILS=$(grep -c 'Failed To Apply' "$dir/daemon.log" || true)
}

# ------------------------------------------------------ case 1: without flag
run_case plain 18779
echo
echo "[plain]  status=$CASE_STATUS patches=$CASE_PATCHES skipped=$CASE_SKIPPED"
[ "${CASE_PATCHES:-0}" -eq 2 ] || fail "[plain] expected both commits as patches, got $CASE_PATCHES"
[ "${CASE_SKIPPED:-0}" -ge 1 ] || fail "[plain] the empty commit was not skipped"
[ "${CASE_APPLY_FAILS:-0}" -eq 0 ] \
    || fail "[plain] $CASE_APPLY_FAILS apply failure(s); the retry loop is still happening"
[ "$CASE_STATUS" = "Reviewed" ] \
    || fail "[plain] patchset settled as '$CASE_STATUS'; it must reach Reviewed"
echo "PASS: empty commit skipped, no apply-failure retries"

# --------------------------------------------------------- case 2: with flag
run_case b4 18780 --b4-cover-letter
echo
echo "[b4]     status=$CASE_STATUS patches=$CASE_PATCHES skipped=$CASE_SKIPPED"
[ "${CASE_PATCHES:-0}" -eq 1 ] \
    || fail "[b4] the tracker should not be a patch at all, got $CASE_PATCHES patch row(s)"
[ "${CASE_APPLY_FAILS:-0}" -eq 0 ] || fail "[b4] $CASE_APPLY_FAILS apply failure(s)"
grep -q 'Adopting empty commit' "$CASE_DIR/daemon.log" \
    || fail "[b4] the tracker was not adopted as a cover letter"

# Dropping a patch row without dropping the part count leaves received_parts
# short of total_parts forever, so the patchset sits in Incomplete and is never
# reviewed. Patch counts alone cannot see that.
[ "$CASE_STATUS" = "Reviewed" ] \
    || fail "[b4] patchset settled as '$CASE_STATUS'; it must still reach Reviewed"
echo "PASS: tracker adopted as cover letter, only the real patch reviewed"

# This series' cover letter is empty — b4 wrote none — so nothing is stored.
# A series where b4 wrote prose would show it here.
BODY=$(query "$CASE_DIR/t.db" \
    "SELECT COALESCE(LENGTH(body), 0) FROM messages WHERE message_id = (SELECT cover_letter_message_id FROM patchsets LIMIT 1)")
echo "[b4]     stored cover letter: ${BODY:-0} bytes (0 expected for this series)"

echo
echo "ALL CHECKS PASSED"
