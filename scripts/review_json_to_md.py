#!/usr/bin/env python3
"""Convert a kreview/korcreview patch review into GitHub-flavored Markdown, or
extract structured per-line comments for posting as inline GitHub PR review comments.

Accepts either the JSON export or the plain-text report (the same inline review
text, already unwrapped, preceded by a `Patchset Details:` header block); the
input format is detected automatically.

Usage:
    review_json_to_md.py patch2.json [-o patch2_review.md]
    review_json_to_md.py review.txt  [-o review.md]
    review_json_to_md.py patch2.json --mode comments [-o patch2_comments.json]

If -o is omitted, writes alongside the input as <input>_review.md / <input>_comments.json.
"""
import argparse
import json
import re
import sys
from pathlib import Path

PATCH_HEADER_RE = re.compile(r'^--- Patch \[(\d+)\]: (.*) ---$')
SEVERITY_RE = re.compile(r'^\[Severity: (.*)\]$')
DIFF_GIT_RE = re.compile(r'^diff --git a/(\S+) b/(\S+)')
HUNK_RE = re.compile(r'^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@')

# Plain-text report scaffolding: header block, per-patch summary blocks, and the
# caret annotations the reviewer uses to point at a column of the line above.
SECTION_RE = re.compile(r'^([A-Z][A-Za-z ]+?)(?: \((\d+)\))?:$')
KV_RE = re.compile(r'^  (\w[\w ]*?):\s+(.*?)\s*$')
PATCH_LIST_RE = re.compile(r'^  \[(\d+)\] (.+?)(?: \(([^()]*)\))?(?: \[([^\[\]]*)\])?$')
SUMMARY_HEAD_RE = re.compile(r'^Patch (\d+): (.*)$')
COUNTS_RE = re.compile(r'^Critical: (\d+) · High: (\d+) · Medium: (\d+) · Low: (\d+)$')
FINDING_RE = re.compile(r'^  \[(Critical|High|Medium|Low)\]\s*(.*?)\s*$')
CARET_RE = re.compile(r'^\s*\^+\s*$')

# Other git-diff header lines that follow a "diff --git" when it is not quoted.
DIFF_EXTENDED_PREFIXES = (
    'index ', 'old mode ', 'new mode ', 'new file mode ', 'deleted file mode ',
    'copy from ', 'copy to ', 'rename from ', 'rename to ', 'similarity index ',
    'dissimilarity index ', '--- ', '+++ ',
)

# GitHub parses <foo@bar.com> as an email autolink and <linux/hwmon.h> as a raw
# HTML tag; in prose we want both rendered literally.
HTML_ISH_RE = re.compile(r'<(?=[A-Za-z/!])')


def _escape_inline(text: str) -> str:
    """Escape the one construct that reliably mangles review prose on GitHub."""
    return HTML_ISH_RE.sub('&lt;', text)


def _is_quote_line(line: str) -> bool:
    return line.startswith('> ') or line == '>'


def _consume_diff_block(lines: list, i: int) -> tuple:
    """Consume one or more '> '-quoted runs into a single diff block,
    collapsing '[ ... ]' gaps between runs into an inline '...' marker
    instead of breaking the fence. Returns (block_lines, next_index)."""
    n = len(lines)
    block = []
    while i < n and _is_quote_line(lines[i]):
        while i < n and _is_quote_line(lines[i]):
            block.append(lines[i][2:] if lines[i].startswith('> ') else '')
            i += 1

        # A caret annotation ("        ^^") points at a column of the line above
        # it, so it has to stay inside the fence or the alignment is lost.
        if i < n and CARET_RE.match(lines[i]):
            block.append(lines[i])
            i += 1
            if i < n and _is_quote_line(lines[i]):
                continue

        # Look past blank lines for a '[ ... ]' gap marker immediately
        # followed (past more blank lines) by another quoted run; if so,
        # keep everything in the same fence instead of splitting it.
        j = i
        while j < n and lines[j].strip() == '':
            j += 1
        if j < n and lines[j].strip() == '[ ... ]':
            k = j + 1
            while k < n and lines[k].strip() == '':
                k += 1
            if k < n and _is_quote_line(lines[k]):
                block.append('...')
                i = k
                continue
        break
    return block, i


def _render_preamble(lines: list, i: int) -> tuple:
    """Render the plain-text report's header block -- 'Patchset Details:',
    'Patches (N):' and 'Review Summary:' -- as a title, a metadata table and a
    patch table. Returns (out_lines, next_index).

    Only the indented body of each recognised section is consumed; the first
    line that isn't part of one ends the preamble.
    """
    n = len(lines)
    details, summary, patches = {}, {}, []
    section = None

    while i < n:
        line = lines[i]
        if not line.strip():
            i += 1
            continue

        m = SECTION_RE.match(line)
        if m:
            section = m.group(1)
            i += 1
            continue

        if section in ('Patchset Details', 'Review Summary'):
            m = KV_RE.match(line)
            if m:
                (details if section == 'Patchset Details' else summary)[m.group(1)] = m.group(2)
                i += 1
                continue
        elif section == 'Patches':
            m = PATCH_LIST_RE.match(line)
            if m:
                patches.append(m.groups())
                i += 1
                continue
        break

    if not details and not patches:
        return [], i

    out = [f"# Patch Review: {_escape_inline(details.get('Subject', ''))}".rstrip(': '), '']

    meta = [('Patchset ID', details.get('ID')), ('Author', details.get('Author')),
            ('Status', details.get('Status')), ('Date', details.get('Date')),
            ('Model', summary.get('Model'))]
    meta = [(k, v) for k, v in meta if v]
    if meta:
        out += ['| | |', '|---|---|']
        out += [f"| **{k}** | {_escape_inline(v)} |" for k, v in meta]
        out.append('')

    if patches:
        out += [f"### Patches ({len(patches)})", '',
                '| # | Subject | Status | Result |', '|---|---------|--------|--------|']
        for num, subject, status, result in patches:
            out.append(f"| {num} | {_escape_inline(subject)} | {status or ''} | {result or ''} |")
        out.append('')

    return out, i


def _render_summary_block(lines: list, i: int) -> tuple:
    """Render a per-patch summary ('Patch N: subject' + severity counts + a
    bullet per finding) that precedes each '--- Patch [N] ---' section.
    Returns (out_lines, next_index), or (None, i) if this isn't one.
    """
    n = len(lines)
    if not SUMMARY_HEAD_RE.match(lines[i]):
        return None, i

    j = i + 1
    while j < n and not lines[j].strip():
        j += 1
    m = j < n and COUNTS_RE.match(lines[j])
    if not m:
        return None, i

    crit, high, med, low = m.groups()
    out = [f"**Findings:** Critical {crit} · High {high} · Medium {med} · Low {low}", '']

    i = j + 1
    while i < n:
        if not lines[i].strip():
            i += 1
            continue
        m = FINDING_RE.match(lines[i])
        if not m:
            break
        title = _escape_inline(m.group(2))
        out.append(f"- **{m.group(1)}**" + (f" — {title}" if title else ""))
        i += 1

    out.append('')
    return out, i


def convert_inline_review(text: str) -> str:
    lines = text.split('\n')
    out = []
    pending_summary = []
    i, n = 0, len(lines)
    while i < n:
        line = lines[i]

        if line == 'Patchset Details:':
            block, i = _render_preamble(lines, i)
            out.extend(block)
            continue

        m = PATCH_HEADER_RE.match(line)
        if m:
            out.append(f"## Patch {m.group(1)}: {_escape_inline(m.group(2))}")
            out.append('')
            out.extend(pending_summary)
            pending_summary = []
            i += 1
            continue

        block, next_i = _render_summary_block(lines, i)
        if block is not None:
            pending_summary, i = block, next_i
            continue

        m = SEVERITY_RE.match(line)
        if m:
            out.append(f"\n**Severity: {m.group(1)}**\n")
            i += 1
            continue

        if _is_quote_line(line):
            block, i = _consume_diff_block(lines, i)
            out.append('```diff')
            out.extend(block)
            out.append('```')
            continue

        # An unquoted 'diff --git' header (the reviewer drops the quoting when a
        # file's hunks are all elided) still needs a fence.
        if DIFF_GIT_RE.match(line):
            block = [line]
            i += 1
            while i < n and lines[i].startswith(DIFF_EXTENDED_PREFIXES) \
                    and not PATCH_HEADER_RE.match(lines[i]):
                block.append(lines[i])
                i += 1
            out.append('```diff')
            out.extend(block)
            out.append('```')
            continue

        if line.strip() == '[ ... ]':
            out.append('*(...)*')
            i += 1
            continue

        out.append(_escape_inline(line))
        i += 1

    out.extend(pending_summary)
    return '\n'.join(out)


def _best_reviews_by_patch(data: dict) -> dict:
    """A patch may have multiple review attempts (retries after failure).
    Pick the one with content, preferring status == 'Reviewed', then the
    most recently created."""
    best = {}
    for r in data.get('reviews', []):
        if not r.get('inline_review'):
            continue
        pid = r['patch_id']
        cur = best.get(pid)
        if cur is None:
            best[pid] = r
            continue
        cur_ok = cur.get('status') == 'Reviewed'
        r_ok = r.get('status') == 'Reviewed'
        if r_ok and not cur_ok:
            best[pid] = r
        elif r_ok == cur_ok and r.get('created_at', 0) > cur.get('created_at', 0):
            best[pid] = r
    return best


def build_markdown(data: dict) -> str:
    patches = {p['id']: p for p in data.get('patches', [])}
    reviews = _best_reviews_by_patch(data)
    order = [p['id'] for p in sorted(patches.values(), key=lambda p: p['part_index'])]

    parts = [f"# Patch Review: {data.get('subject', '')}\n"]
    for pid in order:
        r = reviews.get(pid)
        if not r:
            continue
        parts.append(convert_inline_review(r['inline_review']))
        parts.append('\n---\n')
    return '\n'.join(parts)


def _wrap_code_excerpts(body_lines: list) -> str:
    """Wrap contiguous tab-indented code excerpts (used inline in prose,
    not diff quotes) in plain code fences for readable GitHub rendering."""
    out = []
    i, n = 0, len(body_lines)
    while i < n:
        if body_lines[i].startswith('\t'):
            block = []
            while i < n and body_lines[i].startswith('\t'):
                block.append(body_lines[i])
                i += 1
            out.append('```')
            out.extend(block)
            out.append('```')
            continue
        out.append(body_lines[i])
        i += 1
    return '\n'.join(out).strip('\n')


def extract_comments_from_review(patch_id, subject: str, text: str) -> list:
    """Parse one patch's inline_review text into a list of comment dicts,
    each anchored to the last diff line quoted immediately before it:
        {patch_id, subject, file, side, line, severity, body}
    `side`/`line` are None when a comment isn't anchored to a specific line
    (e.g. a general remark on the commit message, before any diff is quoted).
    """
    lines = text.split('\n')
    n = len(lines)
    i = 0
    comments = []
    current_file = None
    old_ln = new_ln = None
    last_line = None  # (old_ln_or_None, new_ln_or_None)

    while i < n:
        line = lines[i]

        if line.startswith('> ') or line == '>':
            content = line[2:] if line.startswith('> ') else ''

            m = DIFF_GIT_RE.match(content)
            if m:
                current_file = m.group(2)
                old_ln = new_ln = None
                last_line = None
                i += 1
                continue

            if content.startswith(('index ', '--- a/', '+++ b/', '--- /dev/null', '+++ /dev/null')):
                i += 1
                continue

            m = HUNK_RE.match(content)
            if m:
                old_ln, new_ln = int(m.group(1)), int(m.group(2))
                i += 1
                continue

            if old_ln is not None:
                if content.startswith('+'):
                    last_line = (None, new_ln)
                    new_ln += 1
                elif content.startswith('-'):
                    last_line = (old_ln, None)
                    old_ln += 1
                else:
                    last_line = (old_ln, new_ln)
                    old_ln += 1
                    new_ln += 1
            i += 1
            continue

        # The reviewer drops the '> ' quoting on a 'diff --git' header when all
        # of that file's hunks are elided. Track it anyway, or every comment
        # after it anchors to the previous file with this file's line numbers.
        m = DIFF_GIT_RE.match(line)
        if m:
            current_file = m.group(2)
            old_ln = new_ln = None
            last_line = None
            i += 1
            continue

        m = SEVERITY_RE.match(line)
        if m:
            severity = m.group(1)
            i += 1
            body_lines = []
            while i < n and not (lines[i].startswith('> ') or lines[i] == '>') \
                    and not SEVERITY_RE.match(lines[i]) \
                    and not PATCH_HEADER_RE.match(lines[i]):
                body_lines.append(lines[i])
                i += 1
            body = _wrap_code_excerpts(body_lines)

            if last_line and last_line[1] is not None:
                side, target = 'RIGHT', last_line[1]
            elif last_line and last_line[0] is not None:
                side, target = 'LEFT', last_line[0]
            else:
                side, target = None, None

            comments.append({
                'patch_id': patch_id,
                'subject': subject,
                'file': current_file,
                'side': side,
                'line': target,
                'severity': severity,
                'body': body,
            })
            continue

        i += 1

    return comments


def extract_all_comments(data: dict) -> list:
    patches = {p['id']: p for p in data.get('patches', [])}
    reviews = sorted(
        _best_reviews_by_patch(data).values(),
        key=lambda r: patches.get(r['patch_id'], {}).get('part_index', 0),
    )
    all_comments = []
    for r in reviews:
        p = patches.get(r['patch_id'], {})
        all_comments.extend(
            extract_comments_from_review(r['patch_id'], p.get('subject', ''), r['inline_review'])
        )
    return all_comments


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('input', type=Path, help='kreview JSON export file')
    ap.add_argument('-o', '--output', type=Path, default=None,
                     help='output path (default: <input>_review.md or <input>_comments.json)')
    ap.add_argument('--mode', choices=['markdown', 'comments'], default='markdown',
                     help="'markdown' (default) renders the full review; "
                          "'comments' extracts a JSON list of {file, side, line, severity, body} "
                          "records suitable for posting as inline GitHub PR review comments")
    args = ap.parse_args()

    raw = args.input.read_text()
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        # Plain-text report: one already-unwrapped inline review for the whole
        # patchset, so there is nothing to select a best review attempt from.
        if args.mode == 'comments':
            comments = extract_comments_from_review(None, '', raw)
            out_path = args.output or args.input.with_name(args.input.stem + '_comments.json')
            out_path.write_text(json.dumps(comments, indent=2))
            unanchored = sum(1 for c in comments if c['line'] is None)
            print(f"Wrote {out_path} ({len(comments)} comments, {unanchored} unanchored)", file=sys.stderr)
            return
        md = convert_inline_review(raw)
        out_path = args.output or args.input.with_name(args.input.stem + '_review.md')
        out_path.write_text(md)
        print(f"Wrote {out_path} ({len(md)} chars)", file=sys.stderr)
        return

    if args.mode == 'comments':
        comments = extract_all_comments(data)
        out_path = args.output or args.input.with_name(args.input.stem + '_comments.json')
        out_path.write_text(json.dumps(comments, indent=2))
        unanchored = sum(1 for c in comments if c['line'] is None)
        print(f"Wrote {out_path} ({len(comments)} comments, {unanchored} unanchored)", file=sys.stderr)
        return

    md = build_markdown(data)
    out_path = args.output or args.input.with_name(args.input.stem + '_review.md')
    out_path.write_text(md)
    print(f"Wrote {out_path} ({len(md)} chars)", file=sys.stderr)


if __name__ == '__main__':
    main()
