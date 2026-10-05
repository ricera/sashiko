#!/bin/bash
# End-to-end check that an empty commit in a submitted range is skipped.
#
# Range-submitting a b4 prep branch includes its empty tracker commit as the
# first patch of the range. This script builds a throwaway repository with a
# base commit, an empty tracker commit and one real commit, submits the range
# to a daemon backed by a stub model, and asserts that:
#   - both commits are stored as patches;
#   - the tracker patch is Skipped;
#   - the tracker patch has no reviews row, so no worker was spawned for it;
#   - the real commit is part 2, is Reviewed and has a Reviewed reviews row,
#     so the skip did not swallow a patch that changes something;
#   - the patchset still ends Reviewed.
#
# Before asserting, it prints what the daemon did with both patches, so a run
# against a build that predates the skip shows how the tracker was handled.
#
# A skipped patch counts as a success in the patchset's tally, so the
# patchset ends Reviewed even when every patch is skipped. The checks on the
# real commit are what catch a skip that is too broad.
#
# Needs a debug build (cargo build), git, python3 and curl. It first checks
# its own query helper and the premise that the tracker changes no files;
# --self-test stops after that, and needs only git and python3.
#
# Safety rules:
#   - Only ever kill PIDs this script started and recorded itself, and only
#     after checking the process's command line still carries our marker.
#   - Never pass a negative PID to kill; a negative PID means a whole process
#     group, and `-0` is the caller's own group.
#   - Never use `pkill -f`; the pattern matches this script's own command line.
#   - `timeout` on every background process is the real backstop.

set -u

# A caller such as a git hook exports these, and they would point every git
# command below at the caller's repository instead of the throwaway one.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_PREFIX

PORT=18779
STUB_PORT=19779
WORKDIR=$(mktemp -d /tmp/sashiko-emptycommit.XXXXXX)
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO_ROOT/target/debug/sashiko"
SRC="$WORKDIR/src"

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

# Prints the first column of the first row SQL returns from database DB. An
# SQL NULL prints as an empty line, the same as no row at all, because every
# caller tests for "nothing there" with an empty string and Python would
# otherwise print NULL as the word None.
query_db() {
    python3 - "$1" "$2" <<'PYEOF'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
row = next(c.execute(sys.argv[2]), None)
print("" if row is None or row[0] is None else row[0])
PYEOF
}

query() { query_db "$WORKDIR/t.db" "$1"; }

# Runs git in the throwaway repository with a fixed identity, and with
# signing off, so the caller's global configuration cannot make a commit
# fail or prompt.
src_git() {
    git -C "$SRC" -c user.name=sashiko -c user.email=sashiko@example.com \
        -c commit.gpgsign=false "$@"
}

# Builds the repository the daemon reviews: a base commit, an empty tracker
# commit as b4 prep makes one, and one commit that changes a file. Sets
# BASE_SHA, TRACKER_SHA and HEAD_SHA.
build_repo() {
    mkdir -p "$SRC" || fail "could not create $SRC"
    git init -q -b main "$SRC" || fail "git init failed"
    printf 'int answer(void)\n{\n\treturn 41;\n}\n' > "$SRC/answer.c"
    src_git add answer.c || fail "git add failed"
    src_git commit -q -m "answer: add the answer" || fail "base commit failed"
    BASE_SHA=$(src_git rev-parse HEAD)

    src_git commit -q --allow-empty -m "tracker" || fail "tracker commit failed"
    TRACKER_SHA=$(src_git rev-parse HEAD)

    sed -i 's/41/42/' "$SRC/answer.c"
    src_git commit -q -a -m "answer: return the right answer" || fail "real commit failed"
    HEAD_SHA=$(src_git rev-parse HEAD)
}

# Checks the premises the end-to-end run rests on: the tracker changes no
# file and the real commit does, and query_db prints a NULL as nothing.
self_test() {
    local changed db="$WORKDIR/self-test.db"
    build_repo
    changed=$(src_git diff-tree --no-commit-id --name-only -r "$TRACKER_SHA" | wc -l)
    [ "$changed" -eq 0 ] || fail "self-test: tracker $TRACKER_SHA changes $changed file(s)"
    changed=$(src_git diff-tree --no-commit-id --name-only -r "$HEAD_SHA" | wc -l)
    [ "$changed" -eq 1 ] || fail "self-test: real commit $HEAD_SHA changes $changed file(s), want 1"

    python3 -c 'import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("CREATE TABLE t (v)"); c.execute("INSERT INTO t VALUES (NULL)"); c.commit()' "$db" \
        || fail "self-test: could not build $db"
    [ -z "$(query_db "$db" "SELECT v FROM t")" ] || fail "self-test: query_db printed a NULL as text"
    [ -z "$(query_db "$db" "SELECT v FROM t WHERE 0")" ] || fail "self-test: query_db printed a missing row"
    rm -f "$db"
    echo "PASS: self-test: tracker $TRACKER_SHA changes no files, the real commit changes one"
}

self_test
[ "${1:-}" = "--self-test" ] && exit 0

# ------------------------------------------------------- stub OpenAI-ish model
# Answers every request at once with one object that carries the required
# field of every JSON stage of the kernel review (pre-screen, planning, the
# analysis stages, conflict resolution and verification), each empty. A
# stage reads its own field and ignores the rest, so the real commit's
# review finds nothing and finishes Reviewed.
cat > "$WORKDIR/sashiko-stubai.py" <<'PYEOF'
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

ANSWER = {
    "selected_prompts": [],
    "relevant_stages": [],
    "concerns": [],
    "dismissed_concerns": [],
    "findings": [],
}

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        body = json.dumps({
            "id": "stub", "object": "chat.completion", "model": "stub",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant",
                "content": json.dumps(ANSWER),
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

timeout 420 python3 "$WORKDIR/sashiko-stubai.py" "$STUB_PORT" &
STUB_PID=$!
sleep 2
python3 - "$STUB_PORT" <<'PYEOF' || fail "stub model did not come up"
import socket, sys
s = socket.socket(); s.settimeout(3); s.connect(("127.0.0.1", int(sys.argv[1])))
PYEOF
echo "stub model listening on $STUB_PORT (pid $STUB_PID)"

# -------------------------------------------------------------------- daemon
[ -x "$BIN" ] || fail "build first: cargo build"

# The web UI's static files are read relative to the working directory.
cp -r "$REPO_ROOT/static" "$WORKDIR/"
sed -e "s|^url = \"sashiko.db\"|url = \"$WORKDIR/t.db\"|" \
    -e "s|^repository_path = .*|repository_path = \"$SRC\"|" \
    -e "s|^provider = \"gemini\"|provider = \"openai-compatible\"|" \
    "$REPO_ROOT/Settings.toml" > "$WORKDIR/Settings.toml"
cat >> "$WORKDIR/Settings.toml" <<EOF

[ai.openai_compat]
base_url = "http://127.0.0.1:$STUB_PORT/v1"
EOF

# exec, so the recorded PID is the timeout wrapper itself, which passes a
# SIGTERM on to the daemon.
( cd "$WORKDIR" && OPENAI_API_KEY=stub exec timeout 400 "$BIN" --port "$PORT" \
    > "$WORKDIR/daemon.log" 2>&1 ) &
DAEMON_PID=$!

for _ in $(seq 1 60); do
    curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
    sleep 1
done
curl -fsS --max-time 3 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 \
    || fail "daemon did not start"
echo "daemon up on $PORT (pid $DAEMON_PID)"

# Submission is gated on the local operator token, which the daemon writes
# beside its database.
TOKEN_FILE="$WORKDIR/.sashiko-local-token"
for _ in $(seq 1 10); do [ -s "$TOKEN_FILE" ] && break; sleep 1; done
[ -s "$TOKEN_FILE" ] || fail "no local operator token at $TOKEN_FILE"

# ------------------------------------------------------------ submit the range
# No repo is named, so the fetcher finds every commit in repository_path and
# fetches nothing.
curl -fsS --max-time 10 -X POST "http://127.0.0.1:$PORT/api/submit" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $(cat "$TOKEN_FILE")" \
    -d "{\"type\":\"remote-range\",\"sha\":\"$BASE_SHA..$HEAD_SHA\"}" >/dev/null \
    || fail "submit rejected"
echo "submitted $BASE_SHA..$HEAD_SHA"

# Waits for the patchset to leave every status the server counts as
# unfinished (UNFINISHED_PATCHSET_STATUSES in src/server.rs).
STATUS=""
for _ in $(seq 1 150); do
    STATUS=$(query "SELECT status FROM patchsets ORDER BY id DESC LIMIT 1")
    case "$STATUS" in ''|Fetching|Pending|Incomplete|"In Review") ;; *) break ;; esac
    sleep 2
done

# ------------------------------------------------- what happened to the tracker
PATCHES=$(query "SELECT COUNT(*) FROM patches")
TRACKER_ID=$(query "SELECT id FROM patches WHERE message_id LIKE '%$TRACKER_SHA%'")
TRACKER_PART=$(query "SELECT part_index FROM patches WHERE id = '$TRACKER_ID'")
TRACKER_STATUS=$(query "SELECT status FROM patches WHERE id = '$TRACKER_ID'")
TRACKER_REVIEWS=$(query "SELECT COUNT(*) FROM reviews WHERE patch_id = '$TRACKER_ID'")
TRACKER_REVIEW_STATUSES=$(query "SELECT group_concat(status, ',') FROM reviews WHERE patch_id = '$TRACKER_ID'")

REAL_ID=$(query "SELECT id FROM patches WHERE message_id LIKE '%$HEAD_SHA%'")
REAL_PART=$(query "SELECT part_index FROM patches WHERE id = '$REAL_ID'")
REAL_STATUS=$(query "SELECT status FROM patches WHERE id = '$REAL_ID'")
REAL_REVIEWS=$(query "SELECT COUNT(*) FROM reviews WHERE patch_id = '$REAL_ID'")
REAL_REVIEW_STATUSES=$(query "SELECT group_concat(status, ',') FROM reviews WHERE patch_id = '$REAL_ID'")
REAL_REVIEWED=$(query "SELECT COUNT(*) FROM reviews WHERE patch_id = '$REAL_ID' AND status = 'Reviewed'")

echo
echo "patchset status:          ${STATUS:-none}"
echo "patch rows:               ${PATCHES:-0}"
echo "tracker part:             ${TRACKER_PART:-none}"
echo "tracker status:           ${TRACKER_STATUS:-none}"
echo "tracker reviews rows:     ${TRACKER_REVIEWS:-0} (${TRACKER_REVIEW_STATUSES:-none})"
echo "real commit part:         ${REAL_PART:-none}"
echo "real commit status:       ${REAL_STATUS:-none}"
echo "real commit reviews rows: ${REAL_REVIEWS:-0} (${REAL_REVIEW_STATUSES:-none})"
echo

# ------------------------------------------------------------------ assertions
[ "${PATCHES:-0}" -eq 2 ] || fail "expected 2 patch rows, got ${PATCHES:-0}"
[ -n "$TRACKER_ID" ] || fail "no patch row for the tracker $TRACKER_SHA"
[ "$TRACKER_PART" = "1" ] || fail "tracker is part ${TRACKER_PART:-none}, want part 1"
echo "PASS: both commits stored as patches, the tracker as part 1"

[ "$TRACKER_STATUS" = "Skipped" ] \
    || fail "tracker patch is '${TRACKER_STATUS:-none}', want Skipped"
[ "${TRACKER_REVIEWS:-0}" -eq 0 ] \
    || fail "tracker has ${TRACKER_REVIEWS} reviews row(s) (${TRACKER_REVIEW_STATUSES}); a worker reviewed it"
echo "PASS: tracker Skipped with no reviews row, so no worker was spawned"

[ -n "$REAL_ID" ] || fail "no patch row for the real commit $HEAD_SHA"
[ "$REAL_PART" = "2" ] || fail "real commit is part ${REAL_PART:-none}, want part 2"
[ "$REAL_STATUS" = "Reviewed" ] \
    || fail "real commit patch is '${REAL_STATUS:-none}', want Reviewed"
[ "${REAL_REVIEWED:-0}" -ge 1 ] \
    || fail "real commit has no Reviewed reviews row (${REAL_REVIEWS:-0} row(s): ${REAL_REVIEW_STATUSES:-none})"
echo "PASS: real commit part 2 Reviewed with a Reviewed reviews row"

[ "$STATUS" = "Reviewed" ] || fail "patchset settled as '${STATUS:-none}', want Reviewed"
echo "PASS: patchset Reviewed"

echo
echo "ALL CHECKS PASSED"
exit 0
