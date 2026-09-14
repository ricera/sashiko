#!/bin/bash
# Runs the stage-progress UI checks in verify_stage_progress_ui.mjs.
#
# static/index.html is shipped as-is with no build step, so nothing else catches
# a broken edit to its inline script until the page is opened in a browser. This
# needs a JavaScript engine and nothing else -- no daemon, no database, under a
# second.
#
# node is not a build dependency of this project, so an absent one is a skip and
# not a failure. VS Code ships one, which is why that path is tried.

set -u

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

find_node() {
    local candidate
    for candidate in node nodejs; do
        if command -v "$candidate" >/dev/null 2>&1; then
            command -v "$candidate"
            return 0
        fi
    done
    # VS Code's remote server bundles one; the hash in the path changes per
    # release, so glob rather than pin.
    for candidate in "$HOME"/.vscode-server/bin/*/node; do
        [ -x "$candidate" ] && { echo "$candidate"; return 0; }
    done
    return 1
}

NODE=$(find_node) || {
    echo "SKIP: no node found; install one to run the UI checks"
    exit 0
}

echo "using $NODE"
exec "$NODE" "$REPO_ROOT/scripts/verify_stage_progress_ui.mjs"
