#!/bin/bash
# End-to-end check that a review's conversation is readable while it runs.
#
# Drives a real review through the daemon with a stub AI provider, then asserts
# that review_log_entries fills up during the run with stage names stored as
# text, that /api/review serves it as `live_log` in the finished log's message
# shape, that the public activity route never carries any of it, that the
# rows are cleared once the review finishes, and that the review it drove
# ended Reviewed, so the run exercised every stage rather than failing early.
#
# The commit reviewed is the newest commit of the kernel checkout that is not
# a merge and that the daemon would send to a worker under the [review]
# section of the settings it runs with. The daemon skips, without spawning a
# worker, a patch whose diff is empty, one that touches only files listed in
# ignore_files, and one above max_lines_changed or max_files_touched. The
# merge commits a kernel checkout usually has at HEAD have an empty diff, and
# its newest commits are often MAINTAINERS updates, so reviewing HEAD, or the
# newest commit that changes a file, would often stream nothing.
#
# Needs a debug build (cargo build), git, python3, curl and the kernel
# checkout at third_party/linux. It first checks its own query helper, stage
# check and commit choice on a small database and a throwaway repository;
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
# command below at the caller's repository instead of the one it names.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_PREFIX

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

# The kinds of value the stored stages have, comma-separated. Rows with no
# stage are left out because the opening system message is stored with none;
# when those are the only rows so far, the aggregate is NULL and query_db
# prints an empty string.
STAGE_KINDS_SQL="SELECT group_concat(DISTINCT typeof(stage)) FROM review_log_entries WHERE stage IS NOT NULL"

# Fails unless every stored stage is a name. An empty list of kinds means no
# entry has a stage yet, which is nothing to check rather than a failure.
check_stage_kinds() {
    case "$1" in
        ''|text) return 0 ;;
        *) echo "stage stored as $1, not text"; return 1 ;;
    esac
}

# How many commits back from HEAD reviewable_commit looks before giving up.
PICK_LIMIT=50

# Reads the output of git show --format= for one commit on stdin and exits 0
# when the daemon would send it to a worker under the [review] section of
# settings file argv[1]. It applies the daemon's own rules to the same text:
# the files are the a/ paths of the diff --git headers (extract_files_from_diff
# in baseline.rs), the lines changed are those that start with + or - but not
# with +++ or --- (the size check in reviewer.rs), and a patch is skipped when
# it has no file, when every file starts with an ignore_files entry, or when
# either count is above its limit. A limit the file leaves out takes the
# default settings.rs gives it, as the daemon's does.
DAEMON_WOULD_REVIEW_PY='
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    review = tomllib.load(f).get("review", {})
ignored = review.get("ignore_files", [])
max_lines = review.get("max_lines_changed", 10000)
max_files = review.get("max_files_touched", 200)
files, lines = set(), 0
for line in sys.stdin.buffer.read().decode("utf-8", "replace").splitlines():
    if line.startswith("diff --git a/") and " " in line[13:]:
        files.add(line[13:].split(" ", 1)[0])
    elif (line.startswith("+") and not line.startswith("+++")) or (
        line.startswith("-") and not line.startswith("---")
    ):
        lines += 1
if not files:
    sys.exit("empty diff")
if all(any(f.startswith(i) for i in ignored) for f in files):
    sys.exit("touches only ignored files")
if lines > max_lines or len(files) > max_files:
    sys.exit(f"exceeds size limits: {lines} lines, {len(files)} files")
'

# Prints the newest commit reachable from HEAD in repository REPO that is not
# a merge and that the daemon would review under settings file SETTINGS, and
# fails when none of the newest PICK_LIMIT commits that are not merges is one.
reviewable_commit() {
    local repo="$1" settings="$2" sha
    for sha in $(git -C "$repo" rev-list --no-merges -n "$PICK_LIMIT" HEAD 2>/dev/null); do
        if git -C "$repo" show --format= "$sha" \
            | python3 -c "$DAEMON_WOULD_REVIEW_PY" "$settings" 2>/dev/null; then
            echo "$sha"
            return 0
        fi
    done
    return 1
}

# Builds, in the throwaway repository REPO, a history whose HEAD is a merge
# with an empty diff, as the kernel checkout's usually is, and whose newest
# commits that are not merges are each skipped by the daemon for a different
# reason under the settings self_test_commit_choice writes: an empty commit,
# one touching only ignored files, one a line over max_lines_changed and one
# a file over max_files_touched. Sets COMMIT_CHOICE_WANT to the one commit
# below them that the daemon would review. That commit touches MAINTAINERS
# beside a code file and sits exactly at both limits, so a check that skips
# any commit touching an ignored file, or treats a limit as exclusive, is
# caught as well.
build_commit_choice_repo() {
    local repo="$1" n=0
    commit() {
        local when
        n=$((n + 1))
        when=$(printf '2026-01-01T00:00:%02dZ' "$n")
        GIT_AUTHOR_DATE="$when" GIT_COMMITTER_DATE="$when" \
            git -C "$repo" -c user.name=sashiko -c user.email=sashiko@example.com \
            -c commit.gpgsign=false "$@" || fail "self-test: git $1 number $n failed"
    }
    # Appends one line to each named file, once per time it is named.
    change() {
        local f
        for f in "$@"; do
            mkdir -p "$(dirname "$repo/$f")" && echo "$n" >> "$repo/$f" \
                || fail "self-test: could not change $f"
        done
        git -C "$repo" add -A || fail "self-test: git add failed"
    }
    git init -q -b main "$repo" || fail "self-test: git init failed"
    change answer.c MAINTAINERS
    commit commit -q -m "answer: add the answer"
    change answer.c answer.c answer.c MAINTAINERS
    commit commit -q -m "answer: return the right answer"
    COMMIT_CHOICE_WANT=$(git -C "$repo" rev-parse HEAD)
    change big.c big.c big.c big.c big.c
    commit commit -q -m "big: one line over the limit"
    change a.c b.c MAINTAINERS
    commit commit -q -m "tree: one file over the limit"
    change LICENSES/GPL MAINTAINERS
    commit commit -q -m "LICENSES: only ignored files"
    git -C "$repo" branch -q side || fail "self-test: branch failed"
    commit commit -q --allow-empty -m "tracker"
    git -C "$repo" checkout -q side || fail "self-test: checkout failed"
    commit commit -q --allow-empty -m "side: nothing"
    git -C "$repo" checkout -q main || fail "self-test: checkout failed"
    commit merge -q --no-ff -m "Merge side" side
}

# Checks reviewable_commit on the history build_commit_choice_repo makes, so
# a choice of HEAD, of the newest commit that changes a file, or of one the
# daemon skips for its files or its size is caught without starting a daemon.
self_test_commit_choice() {
    local repo="$WORKDIR/self-test-repo" settings="$WORKDIR/self-test.toml" got
    printf '[review]\n%s\n%s\n%s\n' 'ignore_files = ["MAINTAINERS", "LICENSES/"]' \
        'max_lines_changed = 4' 'max_files_touched = 2' > "$settings"
    build_commit_choice_repo "$repo"
    [ -n "$(git -C "$repo" rev-parse -q --verify HEAD^2)" ] \
        || fail "self-test: HEAD of the throwaway repository is not a merge"
    [ -z "$(git -C "$repo" show --format= HEAD)" ] \
        || fail "self-test: the merge at HEAD has a non-empty diff"
    got=$(reviewable_commit "$repo" "$settings") || fail "self-test: no reviewable commit found"
    [ "$got" = "$COMMIT_CHOICE_WANT" ] \
        || fail "self-test: picked '$(git -C "$repo" log -1 --format=%s "$got")', want the newest commit the daemon would review"
    rm -rf "$repo" "$settings"
    echo "PASS: self-test of the commit choice: skips merges, empty commits, ignored files and oversized commits"
}

# Checks query_db and check_stage_kinds against a database built here, so a
# change that lets a log with no stage yet fail the run, or lets a stage
# stored as a number pass, is caught without starting a daemon.
self_test() {
    local db="$WORKDIR/self-test.db" out
    put() {
        python3 -c 'import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); c.execute(sys.argv[2]); c.commit()' "$db" "$1" \
            || fail "self-test: could not run: $1"
    }
    expect_stage_check() {
        local want="$1" label="$2" kinds
        kinds=$(query_db "$db" "$STAGE_KINDS_SQL")
        if check_stage_kinds "$kinds" >/dev/null; then out=pass; else out=fail; fi
        [ "$out" = "$want" ] \
            || fail "self-test: $label: stage check gave $out on kinds '$kinds', want $want"
    }

    put "CREATE TABLE review_log_entries (id INTEGER PRIMARY KEY, stage)"
    expect_stage_check pass "an empty table"
    put "INSERT INTO review_log_entries (stage) VALUES (NULL)"
    [ -z "$(query_db "$db" "SELECT stage FROM review_log_entries")" ] \
        || fail "self-test: query_db printed a NULL as text"
    expect_stage_check pass "only a system message with no stage"
    put "INSERT INTO review_log_entries (stage) VALUES ('Analysis')"
    expect_stage_check pass "a stage stored as a name"
    put "INSERT INTO review_log_entries (stage) VALUES (3)"
    expect_stage_check fail "a stage stored as a number beside a name"
    put "DELETE FROM review_log_entries WHERE stage = 'Analysis'"
    expect_stage_check fail "a stage stored as a number only"
    rm -f "$db"
    echo "PASS: self-test of the query helper and the stage check"
}

self_test
self_test_commit_choice
[ "${1:-}" = "--self-test" ] && exit 0

# ------------------------------------------------------- stub OpenAI-ish model
# Answers slowly so the review stays in flight long enough to observe. Every
# answer is one object that carries the required field of every JSON stage of
# the kernel review (pre-screen, planning, the analysis stages, conflict
# resolution and verification), each empty, as the stub of
# verify_empty_commit.sh does. A stage reads its own field and ignores the
# rest, so the review finds nothing and finishes Reviewed. An answer missing
# a stage's field fails that stage's validation, and a review that fails at
# its first stage streams too little to check.
cat > "$WORKDIR/sashiko-stubai.py" <<'PYEOF'
import json, sys, time
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
        n = int(self.headers.get("content-length", 0))
        self.rfile.read(n)
        time.sleep(3)                      # keep the review visibly running
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
git -C "$REPO_ROOT/third_party/linux" rev-parse -q --verify HEAD >/dev/null 2>&1 \
    || fail "no local kernel checkout to review"

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

# Chosen under the settings the daemon is about to read, so the skip rules
# and limits the choice applies are the ones the daemon applies.
REVIEW_SHA=$(reviewable_commit "$REPO_ROOT/third_party/linux" "$WORKDIR/Settings.toml") \
    || fail "none of the newest $PICK_LIMIT commits of the kernel checkout that are not merges is one the daemon would review"

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
    -d "{\"type\":\"remote\",\"sha\":\"$REVIEW_SHA\"}" >/dev/null \
    || fail "submit rejected"
echo "submitted $REVIEW_SHA, the newest commit that is not a merge and that the daemon would review"

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
        KINDS=$(query "$STAGE_KINDS_SQL")
        WHY=$(check_stage_kinds "$KINDS") || fail "$WHY"
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

    case "$STATUS" in Reviewed|Partial|Failed|Cancelled|"Failed To Apply"|Skipped) break ;; esac
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

# ------------------------------------------------- the review ran to the end
# The loop above stops at any finished status, and a review that failed at
# its first stage still streams a few entries, so the checks above pass
# without this one for a run that never reached the later stages.
REVIEW_STATUS=$(query "SELECT status FROM reviews WHERE id = ${RID:-0}")
[ "$REVIEW_STATUS" = "Reviewed" ] \
    || fail "review $RID ended '${REVIEW_STATUS:-none}', want Reviewed: $(query "SELECT result_description FROM reviews WHERE id = ${RID:-0}")"
[ "$STATUS" = "Reviewed" ] || fail "patchset ended '${STATUS:-none}', want Reviewed"
echo "PASS: the review and its patchset ended Reviewed"

echo
echo "ALL CHECKS PASSED"
exit 0
