// Checks how live activity is routed into the web UI: stage-level entries onto
// the patch they describe, everything else into the patchset-wide row.
//
// static/index.html has no build step and no other test coverage, so this is the
// only thing standing between a bad edit and a page that silently renders
// nothing. Run via scripts/verify_stage_progress_ui.sh, which finds a node.
//
// The functions are read out of the shipped page rather than copied here: a
// harness with its own copy would keep passing after the page stopped working.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const src = fs.readFileSync(path.join(REPO, 'static/index.html'), 'utf8');
const script = [...src.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/g)]
    .map(m => m[1]).join('\n');

// Extracts one top-level function's source.
//
// Brace counting does not survive this file: the render functions are mostly
// template literals, and a brace inside a string or a `${}` is indistinguishable
// from a structural one. Every function here sits at a known indent and closes
// with a lone `}` at that same indent, which is unambiguous.
function grab(name) {
    let start = script.indexOf(`function ${name}(`);
    if (start < 0) throw new Error(`no function ${name}`);
    // Keep the `async` marker; dropping it changes what the function is.
    if (script.slice(start - 6, start) === 'async ') start -= 6;

    const lineStart = script.lastIndexOf('\n', start) + 1;
    const indent = script.slice(lineStart, start).match(/^\s*/)[0];
    const closer = `\n${indent}}`;
    const end = script.indexOf(closer, start);
    if (end < 0) throw new Error(`no close for ${name} at indent ${indent.length}`);
    return script.slice(start, end + closer.length);
}

const NAMES = ['escapeHtml', 'formatDuration', 'describeStageWait', 'summarizeReason',
               'renderLiveStageRow', 'paintStageProgress', 'refreshActivity',
               'stopActivityPolling', 'renderReviewCard', 'hostForPatch',
               'stageBreakdownLabel'];
const ctx = {};
new Function('ctx', NAMES.map(grab).join('\n\n') +
    `\n;` + NAMES.map(n => `ctx.${n} = ${n};`).join(''))(ctx);

// ---- minimal DOM ---------------------------------------------------------
// Enough of one to exercise the routing: ids, a class/tag querySelector, and
// element creation, since hosts are now built on demand.
class El {
    constructor(id, tag = 'div') {
        this.id = id; this.tag = tag; this.style = {}; this.attrs = new Map();
        this.children = []; this._html = ''; this.textContent = ''; this.title = '';
        this.className = ''; this.open = false;
    }
    get innerHTML() { return this._html; }
    set innerHTML(v) {
        this._html = v;
        // Only the shape hostForPatch builds; enough for paintStageProgress to
        // find its summary and body.
        if (v.includes('stage-progress-body')) {
            const summary = new El(null, 'summary');
            const body = new El(null, 'div'); body.cls = 'stage-progress-body';
            this.children = [summary, body];
        }
    }
    setAttribute(name, value) { this.attrs.set(name, value); }
    get classList() { return { contains: c => this.className.split(/\s+/).includes(c) }; }
    querySelector(sel) {
        return this.children.find(c => sel.startsWith('.')
            ? c.cls === sel.slice(1) : c.tag === sel) || null;
    }
    insertBefore(node, ref) {
        const i = this.children.indexOf(ref);
        this.children.splice(i < 0 ? this.children.length : i, 0, node);
        register(node);
    }
    appendChild(node) { this.children.push(node); register(node); }
    get nextSibling() { return this._next || null; }
}
const byId = new Map();
const all = [];
function register(node) {
    if (node.id) byId.set(node.id, node);
    if (node.attrs.has('data-stage-progress') && !all.includes(node)) all.push(node);
}
function mkHost(patchId) {
    const host = new El(`stage-progress-${patchId}`);
    host.setAttribute('data-stage-progress', '');
    host.style.display = 'none';
    const summary = new El(null, 'summary');
    const body = new El(null, 'div'); body.cls = 'stage-progress-body';
    host.children.push(summary, body);
    register(host);
    return host;
}
// The block a patch's card lives in. Present for every patch on the page, even
// one with no reviews yet -- which is the case that used to fall back to the
// patchset-wide row.
function mkPatchBlock(patchId) {
    const block = new El(`patch-${patchId}`);
    const heading = new El(null, 'h3');
    const rest = new El(null, 'p');
    heading._next = rest;
    block.children.push(heading, rest);
    byId.set(block.id, block);
    return block;
}
const row = new El('activity-row'); row.style.display = 'none';
const value = new El('activity-value');
byId.set('activity-row', row); byId.set('activity-value', value);

globalThis.document = {
    getElementById: id => byId.get(id) || null,
    querySelectorAll: sel => sel === '[data-stage-progress]' ? all : [],
    createElement: tag => new El(null, tag),
};
globalThis.escapeHtml = ctx.escapeHtml;
globalThis.formatDuration = ctx.formatDuration;
globalThis.describeStageWait = ctx.describeStageWait;
globalThis.summarizeReason = ctx.summarizeReason;
globalThis.renderLiveStageRow = ctx.renderLiveStageRow;
globalThis.paintStageProgress = ctx.paintStageProgress;
globalThis.hostForPatch = ctx.hostForPatch;
globalThis.stageBreakdownLabel = ctx.stageBreakdownLabel;
globalThis.stopActivityPolling = () => {};

let payload;
globalThis.fetch = async () => ({ ok: true, json: async () => payload });

let failures = 0;
const check = (name, cond, detail = '') => {
    if (cond) { console.log(`  ok   ${name}`); }
    else { failures++; console.log(`  FAIL ${name}${detail ? ' :: ' + detail : ''}`); }
};

// ---- case 1: live stages route to their own patch -------------------------
const h10 = mkHost(10), h11 = mkHost(11);
payload = { live: true, entries: [
    { key: 'patchset:7', phase: { kind: 'reviewing' }, description: 'running review stages',
      age_seconds: 100, idle_seconds: 5, patch_id: null, stage: null },
    { key: 'patchset:7/patch:10/stage:3', patch_id: 10, stage: 3,
      phase: { kind: 'stage', stage: 3, turn: 7, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage 3, turn 7/50 (awaiting model)', age_seconds: 300, idle_seconds: 12 },
    { key: 'patchset:7/patch:10/stage:5', patch_id: 10, stage: 5,
      phase: { kind: 'stage', stage: 5, turn: 2, max_turns: 50,
               waiting: { on: 'tools', names: ['git_grep', 'git_show'] } },
      description: 'stage 5, turn 2/50 (running git_grep, git_show)',
      age_seconds: 60, idle_seconds: 400 },
    { key: 'patchset:7/patch:99/stage:1', patch_id: 99, stage: 1,
      phase: { kind: 'stage', stage: 1, turn: 1, max_turns: 50, waiting: { on: 'queued' } },
      description: 'stage 1, turn 1/50 (queued for a model slot)',
      age_seconds: 30, idle_seconds: 1 },
]};
await ctx.refreshActivity(7);

console.log('case 1: live routing');
check('patch 10 host shown', h10.style.display === '');
check('patch 10 got both its stages',
    h10.querySelector('.stage-progress-body').innerHTML.includes('Stage 3') &&
    h10.querySelector('.stage-progress-body').innerHTML.includes('Stage 5'));
check('stage rows are ordered by stage',
    h10.querySelector('.stage-progress-body').innerHTML.indexOf('Stage 3') <
    h10.querySelector('.stage-progress-body').innerHTML.indexOf('Stage 5'));
check('tool wait is spelled out',
    h10.querySelector('.stage-progress-body').innerHTML.includes('running git_grep, git_show'));
check('stalled stage is flagged',
    h10.querySelector('.stage-progress-body').innerHTML.includes('no progress for 6m 40s'));
check('turn counter shown', h10.querySelector('.stage-progress-body').innerHTML.includes('turn 7/50'));
check('summary counts running stages',
    h10.querySelector('summary').textContent === 'Stage progress (2 running)',
    h10.querySelector('summary').textContent);
check('patch 11 has no activity, stays hidden', h11.style.display === 'none');
check('patchset-wide entry stays in the top row', value.innerHTML.includes('running review stages'));
check('stage entries are NOT duplicated into the top row',
    !value.innerHTML.includes('stage 3, turn 7/50'));
check('offscreen patch 99 falls back to the top row',
    value.innerHTML.includes('stage 1, turn 1/50'));
check('top row visible', row.style.display === '');

// ---- case 2: a stage that finished stops claiming to run ------------------
payload = { live: true, entries: [
    { key: 'patchset:7', phase: { kind: 'reviewing' }, description: 'running review stages',
      age_seconds: 400, idle_seconds: 2, patch_id: null, stage: null },
]};
await ctx.refreshActivity(7);
console.log('case 2: stages finish');
check('host hidden once its stages are gone', h10.style.display === 'none');

// ---- case 3: nothing at all hides the top row ----------------------------
payload = { live: true, entries: [] };
await ctx.refreshActivity(7);
console.log('case 3: idle');
check('top row hidden when there is nothing to say', row.style.display === 'none');

// ---- case 5: a failed stage keeps its row and stops claiming to run -------
// The bug this guards: a stage that failed used to keep reporting the last turn
// it managed, which reads as a hang. Clearing it instead would be wrong the
// other way -- a stage that vanished is indistinguishable from one never run.
//
// Assertions are scoped to one row. Searching the whole table lets a sibling
// row's turn counter, or the tooltip's full untruncated reason, satisfy a check
// that the visible cell should have failed.
function rowFor(html, label) {
    const row = html.split('<tr').find(r => r.includes(`>${label}<`));
    if (!row) throw new Error(`no row for ${label}`);
    return '<tr' + row;
}
const visibleText = row => row.replace(/<[^>]*>/g, ' ');

payload = { live: true, entries: [
    { key: 'patchset:7/patch:10/stage:3', patch_id: 10, stage: 3,
      phase: { kind: 'stage', stage: 3, turn: 7, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage 3, turn 7/50 (awaiting model)', age_seconds: 300, idle_seconds: 12 },
    { key: 'patchset:7/patch:10/stage:2', patch_id: 10, stage: 2,
      phase: { kind: 'stage_failed', stage: 2, cancelled: false,
               reason: 'Session exceeded max turns limit (50)\n\nCaused by:\n    nothing' },
      description: 'stage 2 failed: Session exceeded max turns limit (50)',
      age_seconds: 90, idle_seconds: 90 },
    { key: 'patchset:7/patch:10/stage:6', patch_id: 10, stage: 6,
      phase: { kind: 'stage_failed', stage: 6, cancelled: true,
               reason: 'Session cancelled by supervisor' },
      description: 'stage 6 cancelled: Session cancelled by supervisor',
      age_seconds: 30, idle_seconds: 30 },
]};
await ctx.refreshActivity(7);
const body5 = h10.querySelector('.stage-progress-body').innerHTML;
const failedRow = rowFor(body5, 'Stage 2');

console.log('case 5: failed stages');
check('failed stage keeps a row', body5.includes('>Stage 2<'));
check('failed stage names the reason',
    visibleText(failedRow).includes('failed: Session exceeded max turns limit (50)'));
check('the visible cell shows only the first line',
    !visibleText(failedRow).includes('Caused by'));
check('the full reason survives in the tooltip',
    /title="[^"]*Caused by/.test(failedRow), failedRow);
check('failed stage stops claiming a turn',
    !/turn \d+\/\d+/.test(visibleText(failedRow)), visibleText(failedRow));
check('a still-running sibling keeps its turn counter',
    /turn 7\/50/.test(visibleText(rowFor(body5, 'Stage 3'))));
check('cancelled is not called a failure',
    visibleText(rowFor(body5, 'Stage 6')).includes('cancelled: Session cancelled by supervisor'));
check('summary separates running from stopped',
    h10.querySelector('summary').textContent === 'Stage progress (1 running, 2 stopped)',
    h10.querySelector('summary').textContent);

// ---- case 4: persisted (daemon stopped) ---------------------------------
payload = { live: false, entries: [
    { key: 'patchset:7/patch:10/stage:3', patch_id: 10, stage: 3,
      phase: { kind: 'stage' }, description: 'stage 3, turn 7/50 (awaiting model)',
      updated_at: 1200 },
    { key: 'commit:abc', patch_id: null, stage: null,
      phase: { kind: 'fetching' }, description: 'fetching 2 commit(s) from origin',
      updated_at: 1200 },
]};
await ctx.refreshActivity(7);
console.log('case 4: persisted');
check('persisted stage lands on its patch', h10.style.display === '' &&
    h10.querySelector('.stage-progress-body').innerHTML.includes('stage 3, turn 7/50'));
check('persisted stage does not invent a duration',
    !h10.querySelector('.stage-progress-body').innerHTML.includes('0s'));
check('summary says stopped, not running',
    h10.querySelector('summary').textContent === 'Stage progress (stopped)',
    h10.querySelector('summary').textContent);
check('commit-keyed fetch stays in the top row',
    value.innerHTML.includes('Stopped while:') &&
    value.innerHTML.includes('fetching 2 commit(s) from origin'));

// ---- case 6: the log link is reachable while the review is running --------
// The conversation is streamed as it happens, so the link is the only way in.
// It used to be built unconditionally and then rendered inside a block gated on
// a finished status, so it appeared for every status except the ones streaming
// was added for.
console.log('case 6: log link availability');
for (const status of ['In Review', 'Pending']) {
    const card = ctx.renderReviewCard({ id: 42, status, patch_id: 10 });
    check(`link is present while ${status}`, card.includes('#/log/42'), card);
    check(`link says it is live while ${status}`, card.includes('View Live Log'));
    // Things that genuinely do not exist yet must stay hidden.
    check(`no token count while ${status}`, !card.includes('Tokens used'));
}
const done = ctx.renderReviewCard({ id: 42, status: 'Reviewed', patch_id: 10 });
check('finished review still links to its log', done.includes('#/log/42'));
check('finished review says raw, not live',
    done.includes('View Raw Log') && !done.includes('View Live Log'));
check('finished review still shows its token count', done.includes('Tokens used'));
// A review with no row yet has nothing to link to, and must not emit a dead href.
const noId = ctx.renderReviewCard({ status: 'In Review', patch_id: 10 });
check('no link when there is no review to link to', !noId.includes('#/log/'));

// ---- case 7: a patch whose card did not exist at render time -------------
// The page is built once and the activity is polled, so a review that started
// after the load -- or a retry, which creates a new review row -- has no card.
// Those stages used to fall back to the patchset-wide row, which looks exactly
// like the old behaviour of grouping every stage at the top.
console.log('case 7: hosts built on demand');
const block12 = mkPatchBlock(12);
payload = { live: true, entries: [
    { key: 'patchset:7', phase: { kind: 'reviewing_patches', patches: 2 },
      description: 'reviewing 2 patches', age_seconds: 900, idle_seconds: 3,
      patch_id: null, stage: null },
    { key: 'patchset:7/patch:12', patch_id: 12, stage: null,
      phase: { kind: 'planning', attempt: 1, max_attempts: 4 },
      description: 'planning stages', age_seconds: 20, idle_seconds: 20 },
    { key: 'patchset:7/patch:12/stage:1', patch_id: 12, stage: 1,
      phase: { kind: 'stage', stage: 1, turn: 3, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage 1, turn 3/50 (awaiting model)', age_seconds: 15, idle_seconds: 2 },
]};
await ctx.refreshActivity(7);
const built = document.getElementById('stage-progress-12');
check('a host is built for a patch that had no card', !!built);
check('it is styled as a card, since it stands alone',
    !!built && built.classList.contains('review-card'));
check('it sits directly under the patch title',
    !!built && block12.children.indexOf(built) === 1,
    built ? String(block12.children.indexOf(built)) : 'no host');
const body7 = built ? built.querySelector('.stage-progress-body').innerHTML : '';
check('the patch stages went there, not the top row', body7.includes('Stage 1'));
check('the top row is left with the patchset only',
    value.innerHTML.includes('reviewing 2 patches')
        && !value.innerHTML.includes('stage 1, turn 3/50')
        && !value.innerHTML.includes('planning stages'),
    value.innerHTML);

// ---- case 8: the patch's own phase reads as the patch, not a stage --------
console.log('case 8: per-patch coarse entry');
check('planning is labelled for the patch', body7.includes('>This patch<'));
check('planning does not print a stage number', !body7.includes('Stage ?'));
check('planning shows its own elapsed time', body7.includes('20s'));
const summary7 = built ? built.querySelector('summary').textContent : 'no host';
check('the patch entry is not counted as a running stage',
    summary7 === 'Stage progress (1 running)', summary7);
check('the patch entry sorts above its stages',
    body7.indexOf('This patch') < body7.indexOf('Stage 1'));

// ---- case 9: finished stages stay on the card ----------------------------
// A stage used to vanish the moment it succeeded, so the card showed less and
// less as the review progressed and the completed work only reappeared once the
// whole review ended and the recorded breakdown replaced the live view.
console.log('case 9: completed stages');
payload = { live: true, entries: [
    { key: 'patchset:7/patch:10/stage:1', patch_id: 10, stage: 1,
      phase: { kind: 'stage_done', stage: 1, seconds: 185, turns: 12 },
      description: 'stage 1 done in 3m 5s, 12 turns', age_seconds: 40, idle_seconds: 40 },
    { key: 'patchset:7/patch:10/stage:2', patch_id: 10, stage: 2,
      phase: { kind: 'stage_done', stage: 2, seconds: 20, turns: 1 },
      description: 'stage 2 done in 20s, 1 turn', age_seconds: 5, idle_seconds: 5 },
    { key: 'patchset:7/patch:10/stage:3', patch_id: 10, stage: 3,
      phase: { kind: 'stage', stage: 3, turn: 4, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage 3, turn 4/50 (awaiting model)', age_seconds: 60, idle_seconds: 2 },
    { key: 'patchset:7/patch:10/stage:4', patch_id: 10, stage: 4,
      phase: { kind: 'stage_failed', stage: 4, cancelled: false, reason: 'boom' },
      description: 'stage 4 failed: boom', age_seconds: 10, idle_seconds: 10 },
]};
await ctx.refreshActivity(7);
const body9 = h10.querySelector('.stage-progress-body').innerHTML;
const doneRow = rowFor(body9, 'Stage 1');
check('a finished stage keeps its row', body9.includes('>Stage 1<'));
check('it reports the duration the breakdown will show',
    visibleText(doneRow).includes('3m 5s'), visibleText(doneRow));
check('it reports its turn count', visibleText(doneRow).includes('12 turns'));
check('a single turn is not pluralised',
    visibleText(rowFor(body9, 'Stage 2')).includes('1 turn')
        && !visibleText(rowFor(body9, 'Stage 2')).includes('1 turns'));
check('a finished stage does not claim to be mid-turn',
    !/turn \d+\/\d+/.test(visibleText(doneRow)), visibleText(doneRow));
check('the running stage is still shown as running',
    visibleText(rowFor(body9, 'Stage 3')).includes('turn 4/50'));
check('done, running and stopped are counted apart',
    h10.querySelector('summary').textContent === 'Stage progress (1 running, 2 done, 1 stopped)',
    h10.querySelector('summary').textContent);
check('rows stay in stage order',
    body9.indexOf('>Stage 1<') < body9.indexOf('>Stage 3<')
        && body9.indexOf('>Stage 3<') < body9.indexOf('>Stage 4<'));

// ---- case 10: the finished breakdown's heading -----------------------------
// It used to read "(N stages, run concurrently)" on every card: a fact about
// the system rather than about this review, asserting that the rows do not sum
// without showing it.
console.log('case 10: stage breakdown heading');
const many = ctx.renderReviewCard({
    id: 7, status: 'Reviewed', patch_id: 10,
    stage_durations: [
        { stage: 1, seconds: 123, turns: 4 },
        { stage: 2, seconds: 20, turns: 1 },
        { stage: 3, seconds: 617, turns: 9 },
    ],
});
check('the longest stage is named',
    many.includes('longest 10m 17s'), many.slice(0, 400));
check('the sum is named, so the gap from the review time is visible',
    many.includes('12m 40s summed'));
check('the count survives', many.includes('3 stages overlapping'));
check('the old boilerplate is gone', !many.includes('run concurrently'));

const one = ctx.renderReviewCard({
    id: 7, status: 'Reviewed', patch_id: 10,
    stage_durations: [{ stage: 4, seconds: 90, turns: 2 }],
});
// One stage has nothing to overlap and nothing to sum; the comparison would be
// noise, and "longest 90s, 90s summed" reads as a bug.
check('a single stage just states its duration',
    one.includes('Stage breakdown (1 stage, 90s)'), one.slice(0, 400));

console.log(failures ? `\n${failures} FAILURE(S)` : '\nALL CHECKS PASSED');
process.exit(failures ? 1 : 0);
