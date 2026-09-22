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

// Extracts one top-level array constant, closing with `];` at its own indent.
// Read from the page for the same reason the functions are: a copy here would
// keep passing after the page's own table drifted.
function grabConst(name) {
    const start = script.indexOf(`const ${name} = [`);
    if (start < 0) throw new Error(`no const ${name}`);
    const lineStart = script.lastIndexOf('\n', start) + 1;
    const indent = script.slice(lineStart, start).match(/^\s*/)[0];
    const closer = `\n${indent}];`;
    const end = script.indexOf(closer, start);
    if (end < 0) throw new Error(`no close for const ${name} at indent ${indent.length}`);
    return script.slice(start, end + closer.length);
}

const CONSTS = ['STAGE_ORDER'];
const NAMES = ['escapeHtml', 'formatDuration', 'describeStageWait', 'summarizeReason',
               'stageRank', 'renderStageRow', 'paintStageProgress', 'refreshActivity',
               'stopActivityPolling', 'renderReviewCard', 'hostForPatch',
               'stageTableLabel', 'stageTableParts', 'stageRowsFromRecord',
               'stageRowsFromActivity', 'formatToolCalls', 'applyToolCallCounts',
               'parseSeverityCalibration', 'renderSeverityCalibration',
               'isSpeculativeFinding', 'findingHeadline', 'renderFindingLocations',
               'renderFindingsTable', 'toggleFindingReasoning'];
const ctx = {};
new Function('ctx', CONSTS.map(grabConst).join('\n\n') + '\n\n' +
    NAMES.map(grab).join('\n\n') +
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
        // A browser keeps textContent in step with innerHTML. Without that here,
        // rewriting a check from textContent to innerHTML in the page silently
        // empties textContent in the harness alone, and an assertion on it fails
        // for a reason that does not exist in the product.
        this.textContent = v.replace(/<[^>]*>/g, '');
        // A browser makes elements written as innerHTML reachable by id. The
        // page relies on that: paintStageProgress writes a slot, and
        // applyToolCallCounts finds it with getElementById afterwards. Nodes are
        // recreated rather than reused, as replacing innerHTML does.
        for (const m of v.matchAll(/id="([^"]+)"/g)) {
            const el = new El(m[1], 'span');
            byId.set(m[1], el);
        }
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
// A finished review's recorded table, which renderReviewCard emits as a string.
// The page finds these with an attribute selector to decide that a patch's
// stages are already on screen; the harness has to be able to answer the same
// question, so a test mounts one here.
const recorded = new Map();
function register(node) {
    if (node.id) byId.set(node.id, node);
    if (node.attrs.has('data-stage-progress') && !all.includes(node)) all.push(node);
}
// Mounts the recorded table a review card would have rendered, keyed the way
// the page keys it.
function mkRecorded(patchId) {
    const el = new El(null, 'details');
    el.setAttribute('data-stage-recorded', String(patchId));
    recorded.set(String(patchId), el);
    return el;
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
    querySelector: sel => {
        const m = /^\[data-stage-recorded="(.+)"\]$/.exec(sel);
        return m ? (recorded.get(m[1]) || null) : null;
    },
    createElement: tag => new El(null, tag),
};
globalThis.escapeHtml = ctx.escapeHtml;
globalThis.formatDuration = ctx.formatDuration;
globalThis.describeStageWait = ctx.describeStageWait;
globalThis.summarizeReason = ctx.summarizeReason;
globalThis.renderStageRow = ctx.renderStageRow;
globalThis.stageRowsFromActivity = ctx.stageRowsFromActivity;
globalThis.stageRowsFromRecord = ctx.stageRowsFromRecord;
globalThis.stageTableLabel = ctx.stageTableLabel;
globalThis.stageTableParts = ctx.stageTableParts;
globalThis.paintStageProgress = ctx.paintStageProgress;
globalThis.hostForPatch = ctx.hostForPatch;
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
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      phase: { kind: 'stage', stage: 'execution-flow', turn: 7, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage execution-flow, turn 7/50 (awaiting model)', age_seconds: 300, idle_seconds: 12 },
    { key: 'patchset:7/patch:10/stage:locking', patch_id: 10, stage: 'locking',
      phase: { kind: 'stage', stage: 'locking', turn: 2, max_turns: 50,
               waiting: { on: 'tools', names: ['git_grep', 'git_show'] } },
      description: 'stage locking, turn 2/50 (running git_grep, git_show)',
      age_seconds: 60, idle_seconds: 400 },
    { key: 'patchset:7/patch:99/stage:goal', patch_id: 99, stage: 'goal',
      phase: { kind: 'stage', stage: 'goal', turn: 1, max_turns: 50, waiting: { on: 'queued' } },
      description: 'stage goal, turn 1/50 (queued for a model slot)',
      age_seconds: 30, idle_seconds: 1 },
]};
await ctx.refreshActivity(7);

console.log('case 1: live routing');
check('patch 10 host shown', h10.style.display === '');
check('patch 10 got both its stages',
    h10.querySelector('.stage-progress-body').innerHTML.includes('Stage execution-flow') &&
    h10.querySelector('.stage-progress-body').innerHTML.includes('Stage locking'));
check('stage rows are ordered by stage',
    h10.querySelector('.stage-progress-body').innerHTML.indexOf('Stage execution-flow') <
    h10.querySelector('.stage-progress-body').innerHTML.indexOf('Stage locking'));
check('tool wait is spelled out',
    h10.querySelector('.stage-progress-body').innerHTML.includes('running git_grep, git_show'));
check('stalled stage is flagged',
    h10.querySelector('.stage-progress-body').innerHTML.includes('no progress for 6m 40s'));
check('turn counter shown', h10.querySelector('.stage-progress-body').innerHTML.includes('turn 7/50'));
check('summary counts running stages',
    h10.querySelector('summary').textContent === 'Stages (2 running)',
    h10.querySelector('summary').textContent);
check('patch 11 has no activity, stays hidden', h11.style.display === 'none');
check('patchset-wide entry stays in the top row', value.innerHTML.includes('running review stages'));
check('stage entries are NOT duplicated into the top row',
    !value.innerHTML.includes('stage execution-flow, turn 7/50'));
check('offscreen patch 99 falls back to the top row',
    value.innerHTML.includes('stage goal, turn 1/50'));
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
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      phase: { kind: 'stage', stage: 'execution-flow', turn: 7, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage execution-flow, turn 7/50 (awaiting model)', age_seconds: 300, idle_seconds: 12 },
    { key: 'patchset:7/patch:10/stage:implementation', patch_id: 10, stage: 'implementation',
      phase: { kind: 'stage_failed', stage: 'implementation', cancelled: false,
               reason: 'Session exceeded max turns limit (50)\n\nCaused by:\n    nothing' },
      description: 'stage implementation failed: Session exceeded max turns limit (50)',
      age_seconds: 90, idle_seconds: 90 },
    { key: 'patchset:7/patch:10/stage:security', patch_id: 10, stage: 'security',
      phase: { kind: 'stage_failed', stage: 'security', cancelled: true,
               reason: 'Session cancelled by supervisor' },
      description: 'stage security cancelled: Session cancelled by supervisor',
      age_seconds: 30, idle_seconds: 30 },
]};
await ctx.refreshActivity(7);
const body5 = h10.querySelector('.stage-progress-body').innerHTML;
const failedRow = rowFor(body5, 'Stage implementation');

console.log('case 5: failed stages');
check('failed stage keeps a row', body5.includes('>Stage implementation<'));
check('failed stage names the reason',
    visibleText(failedRow).includes('failed: Session exceeded max turns limit (50)'));
check('the visible cell shows only the first line',
    !visibleText(failedRow).includes('Caused by'));
check('the full reason survives in the tooltip',
    /title="[^"]*Caused by/.test(failedRow), failedRow);
check('failed stage stops claiming a turn',
    !/turn \d+\/\d+/.test(visibleText(failedRow)), visibleText(failedRow));
check('a still-running sibling keeps its turn counter',
    /turn 7\/50/.test(visibleText(rowFor(body5, 'Stage execution-flow'))));
check('cancelled is not called a failure',
    visibleText(rowFor(body5, 'Stage security')).includes('stopped: Session cancelled by supervisor'));
check('summary separates running from stopped',
    h10.querySelector('summary').textContent === 'Stages (1 running, 2 stopped)',
    h10.querySelector('summary').textContent);

// ---- case 4: persisted (daemon stopped) ---------------------------------
payload = { live: false, entries: [
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      phase: { kind: 'stage' }, description: 'stage execution-flow, turn 7/50 (awaiting model)',
      updated_at: 1200 },
    { key: 'commit:abc', patch_id: null, stage: null,
      phase: { kind: 'fetching' }, description: 'fetching 2 commit(s) from origin',
      updated_at: 1200 },
]};
await ctx.refreshActivity(7);
console.log('case 4: persisted');
check('persisted stage lands on its patch', h10.style.display === '' &&
    h10.querySelector('.stage-progress-body').innerHTML.includes('stage execution-flow, turn 7/50'));
check('persisted stage does not invent a duration',
    !h10.querySelector('.stage-progress-body').innerHTML.includes('0s'));
check('summary says stopped, not running',
    h10.querySelector('summary').textContent === 'Stages (stopped)',
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
    // Things that genuinely do not exist yet must stay hidden.
    check(`no token count while ${status}`, !card.includes('Tokens used'));
}
const done = ctx.renderReviewCard({ id: 42, status: 'Reviewed', patch_id: 10 });
check('finished review still links to its log', done.includes('#/log/42'));

// One destination, so one name. Two labels for one href read as two different
// places, and the live and finished views now render the same messages the same
// way, so there is no difference left for the wording to carry.
const running = ctx.renderReviewCard({ id: 42, status: 'In Review', patch_id: 10 });
check('the log link reads the same whatever the status',
    running.includes('>View Log<') && done.includes('>View Log<'),
    running.slice(0, 300));
check('the old split naming is gone',
    !running.includes('View Live Log') && !done.includes('View Raw Log'));
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
    { key: 'patchset:7/patch:12/stage:goal', patch_id: 12, stage: 'goal',
      phase: { kind: 'stage', stage: 'goal', turn: 3, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage goal, turn 3/50 (awaiting model)', age_seconds: 15, idle_seconds: 2 },
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
check('the patch stages went there, not the top row', body7.includes('Stage goal'));
check('the top row is left with the patchset only',
    value.innerHTML.includes('reviewing 2 patches')
        && !value.innerHTML.includes('stage goal, turn 3/50')
        && !value.innerHTML.includes('planning stages'),
    value.innerHTML);

// ---- case 8: the patch's own phase reads as the patch, not a stage --------
console.log('case 8: per-patch coarse entry');
check('planning is labelled for the patch', body7.includes('>This patch<'));
check('planning does not print a stage number', !body7.includes('Stage ?'));
check('planning shows its own elapsed time', body7.includes('20s'));
const summary7 = built ? built.querySelector('summary').textContent : 'no host';
check('the patch entry is not counted as a running stage',
    summary7 === 'Stages (1 running)', summary7);
check('the patch entry sorts above its stages',
    body7.indexOf('This patch') < body7.indexOf('Stage goal'));

// ---- case 9: finished stages stay on the card ----------------------------
// A stage used to vanish the moment it succeeded, so the card showed less and
// less as the review progressed and the completed work only reappeared once the
// whole review ended and the recorded breakdown replaced the live view.
console.log('case 9: completed stages');
// Deliberately not in stage order: stages are identified by name, so the rows
// can only come out in review order if the page ranks them against its table.
// A comparator that silently yields NaN -- which is what subtracting two names
// does -- leaves the input order untouched and would pass a sorted fixture.
payload = { live: true, entries: [
    { key: 'patchset:7/patch:10/stage:resources', patch_id: 10, stage: 'resources',
      phase: { kind: 'stage_failed', stage: 'resources', cancelled: false, reason: 'boom' },
      description: 'stage resources failed: boom', age_seconds: 10, idle_seconds: 10 },
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      phase: { kind: 'stage', stage: 'execution-flow', turn: 4, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage execution-flow, turn 4/50 (awaiting model)', age_seconds: 60, idle_seconds: 2 },
    { key: 'patchset:7/patch:10/stage:goal', patch_id: 10, stage: 'goal',
      phase: { kind: 'stage_done', stage: 'goal', seconds: 185, turns: 12 },
      description: 'stage goal done in 3m 5s, 12 turns', age_seconds: 40, idle_seconds: 40 },
    { key: 'patchset:7/patch:10/stage:implementation', patch_id: 10, stage: 'implementation',
      phase: { kind: 'stage_done', stage: 'implementation', seconds: 20, turns: 1 },
      description: 'stage implementation done in 20s, 1 turn', age_seconds: 5, idle_seconds: 5 },
]};
await ctx.refreshActivity(7);
const body9 = h10.querySelector('.stage-progress-body').innerHTML;
const doneRow = rowFor(body9, 'Stage goal');
check('a finished stage keeps its row', body9.includes('>Stage goal<'));
check('it reports the duration the breakdown will show',
    visibleText(doneRow).includes('3m 5s'), visibleText(doneRow));
check('it reports its turn count', visibleText(doneRow).includes('12 turns'));
check('a single turn is not pluralised',
    visibleText(rowFor(body9, 'Stage implementation')).includes('1 turn')
        && !visibleText(rowFor(body9, 'Stage implementation')).includes('1 turns'));
check('a finished stage does not claim to be mid-turn',
    !/turn \d+\/\d+/.test(visibleText(doneRow)), visibleText(doneRow));
check('the running stage is still shown as running',
    visibleText(rowFor(body9, 'Stage execution-flow')).includes('turn 4/50'));
check('done, running and stopped are counted apart',
    h10.querySelector('summary').textContent === 'Stages (1 running, 2 done, 1 stopped)',
    h10.querySelector('summary').textContent);
check('rows stay in stage order',
    body9.indexOf('>Stage goal<') < body9.indexOf('>Stage execution-flow<')
        && body9.indexOf('>Stage execution-flow<') < body9.indexOf('>Stage resources<'));

// ---- case 10: the finished breakdown's heading -----------------------------
// It used to read "(N stages, run concurrently)" on every card: a fact about
// the system rather than about this review, asserting that the rows do not sum
// without showing it.
console.log('case 10: stage breakdown heading');
const many = ctx.renderReviewCard({
    id: 7, status: 'Reviewed', patch_id: 10,
    stage_durations: [
        { stage: 'goal', seconds: 123, turns: 4 },
        { stage: 'implementation', seconds: 20, turns: 1 },
        { stage: 'execution-flow', seconds: 617, turns: 9 },
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
    stage_durations: [{ stage: 'resources', seconds: 90, turns: 2 }],
});
// One stage has nothing to overlap and nothing to sum; the comparison would be
// noise, and "longest 90s, 90s summed" reads as a bug.
check('a single stage just states its duration',
    one.includes('Stages (1 stage, 90s)'), one.slice(0, 400));

// ---- case 11: tool call counts reach the live element --------------------
// Before a review finishes there is no review card, so the stage-progress
// summary is the only place a count can appear. A count that only ever renders
// on the finished card is invisible for the whole run, which is exactly when a
// climbing number is worth having.
console.log('case 11: live tool call counts');
const h20 = mkHost(20);
ctx.paintStageProgress(h20, [
    { patch_id: 20, stage: 1, description: 'Stage 1', phase: { kind: 'stage_turn' } },
], true);
check('the summary leaves a slot for the count',
    h20.querySelector('summary').innerHTML.includes('id="stage-tool-calls-20"'),
    h20.querySelector('summary').innerHTML);

ctx.applyToolCallCounts([
    { review_id: 5, patch_id: 20, total: 40, reference: 0 },
    { review_id: 6, patch_id: 20, total: 7, reference: 3 },
]);
const slot20 = byId.get('stage-tool-calls-20');
// Summed across attempts: what has this patch cost, not what did one try cost.
check('counts land on the live element and sum across attempts',
    slot20 && slot20.textContent === ' — 47 (3 to the kernel tree) tool calls',
    slot20 && slot20.textContent);

// A patch that never touched the reference tree says so by omission, not by a
// zero that an in-tree review could never move off.
ctx.applyToolCallCounts([{ review_id: 5, patch_id: 20, total: 12, reference: 0 }]);
check('no reference calls means no reference clause',
    byId.get('stage-tool-calls-20').textContent === ' — 12 tool calls',
    byId.get('stage-tool-calls-20').textContent);

check('a payload without counts is ignored rather than clearing the slot',
    (() => {
        ctx.applyToolCallCounts(undefined);
        return byId.get('stage-tool-calls-20').textContent === ' — 12 tool calls';
    })());

// ---- case 12: a failed review must not claim to have succeeded ------------
// Every attempt after the first read "succeeded on attempt N" regardless of
// status, so four consecutive three-hour timeouts each reported success. The
// recorded duration is cumulative across attempts, so a later attempt also has
// to say the figure is a total rather than what that attempt alone cost.
console.log('case 12: failed review cards');
const failedCard = ctx.renderReviewCard({
    id: 9, status: 'Failed', patch_id: 10, attempt: 4, duration_seconds: 43200,
    result: 'Tool error: Review tool timed out (active time exceeded)',
    stage_durations: [
        { stage: 'goal', seconds: 120, turns: 3 },
        { stage: 'implementation', seconds: 300, turns: 6 },
    ],
    stage_failures: [
        { stage: 'locking', reason: 'still running when the review stopped, after 2h 58m and 47 turn(s)', cancelled: true },
    ],
});

check('a failed review never claims to have succeeded',
    !failedCard.includes('succeeded'), failedCard.slice(0, 300));
check('it says it failed, and how long it took',
    failedCard.includes('Failed after 12h'), failedCard.slice(0, 300));
check('the cumulative duration is labelled as a total',
    failedCard.includes('total'), failedCard.slice(0, 300));
check('the attempt is still reported', failedCard.includes('on attempt 4'));
check('the stages that did finish are still listed',
    failedCard.includes('Stage goal') && failedCard.includes('Stage implementation'));
// The whole point of the report: which stage was running when time ran out.
check('the stage that did not finish is named',
    failedCard.includes('Stage locking') && failedCard.includes('2h 58m'),
    failedCard.slice(0, 600));
// It is now a row of the stage table rather than a line in the banner, so the
// wording lives in the status cell. Same claim, one place to read it.
check('an unfinished stage is not called a failure',
    visibleText(rowFor(failedCard, 'Stage locking')).includes('stopped:')
        && !visibleText(rowFor(failedCard, 'Stage locking')).includes('failed:'),
    failedCard.slice(0, 900));
// The stage names and reasons belong to the table now; repeating them in the
// banner made a reader check whether the two lists agreed.
check('the banner does not repeat what the table lists',
    failedCard.split('Stage locking').length === 2, failedCard.slice(0, 900));
check('the coverage gap is stated',
    failedCard.includes('did not complete'), failedCard.slice(0, 600));
check('partial findings are not presented as a full review',
    failedCard.includes('only from the stages that finished'));

// A first-attempt success is the common case and must stay unchanged: no
// attempt number, and no "total" qualifier on a duration that is one run.
const cleanCard = ctx.renderReviewCard({
    id: 10, status: 'Reviewed', patch_id: 11, attempt: 1, duration_seconds: 90,
});
check('a first-attempt review still reads plainly',
    cleanCard.includes('Reviewed in 90s') && !cleanCard.includes('total')
        && !cleanCard.includes('attempt'), cleanCard.slice(0, 300));
check('a retried success still says it succeeded',
    ctx.renderReviewCard({ id: 11, status: 'Reviewed', patch_id: 11, attempt: 2, duration_seconds: 90 })
        .includes('succeeded on attempt 2'));

// ---- case 13: one table, live and recorded -------------------------------
// The same ten stages used to render twice: "Stage breakdown" inside the review
// card, from the database, and "Stage progress" in a card of its own, from the
// activity registry, in a different format. The registry holds a patch's entries
// until the whole patchset ends, so every reviewed patch in a running patchset
// got both.
console.log('case 13: one stage table');

// The anti-drift check, and the reason the row model is shared rather than
// merely similar: a finished stage must render identically whether its numbers
// arrived from the registry a second ago or from the database an hour later.
const fromRecord = ctx.stageRowsFromRecord([{ stage: 'goal', seconds: 574, turns: 8 }], []);
const fromActivity = ctx.stageRowsFromActivity([{
    key: 'patchset:5/patch:19/stage:goal', patch_id: 19, stage: 'goal',
    phase: { kind: 'stage_done', stage: 'goal', seconds: 574, turns: 8 },
    description: 'stage goal done in 9m 34s, 8 turns', age_seconds: 1212,
}], true);
check('a finished stage renders the same from either source',
    ctx.renderStageRow(fromRecord[0]) === ctx.renderStageRow(fromActivity[0]),
    `${ctx.renderStageRow(fromRecord[0])}\n!==\n${ctx.renderStageRow(fromActivity[0])}`);
check('and it is the row the reader wants',
    visibleText(ctx.renderStageRow(fromRecord[0])).includes('9m 34s')
        && visibleText(ctx.renderStageRow(fromRecord[0])).includes('8 turns'),
    ctx.renderStageRow(fromRecord[0]));

// A stopped stage: recorded failures carry no duration, only a reason whose text
// already holds the elapsed time.
const stoppedRow = ctx.stageRowsFromRecord([], [
    { stage: 'locking', reason: 'still running when the review stopped, after 2h 58m', cancelled: true },
])[0];
check('a recorded stopped stage does not invent a duration',
    visibleText(ctx.renderStageRow(stoppedRow)).includes('—'), ctx.renderStageRow(stoppedRow));
check('durations and failures are one list, in stage order',
    ctx.stageRowsFromRecord(
        [{ stage: 'security', seconds: 10, turns: 1 }],
        [{ stage: 'goal', reason: 'x', cancelled: true }],
    ).map(r => r.stage).join(',') === 'goal,security');

// The finished review's table now lives in the review card and says so, which is
// what stops a second card being built beside it.
const recordedCard = ctx.renderReviewCard({
    id: 8, status: 'Reviewed', patch_id: 19,
    stage_durations: [{ stage: 'goal', seconds: 574, turns: 8 }],
});
check('a recorded card marks itself as holding the stages',
    recordedCard.includes('data-stage-recorded="19"'), recordedCard.slice(0, 400));
check('the recorded table carries a status column like the live one',
    visibleText(rowFor(recordedCard, 'Stage goal')).includes('done'),
    rowFor(recordedCard, 'Stage goal'));

// And the live entries for that patch are dropped rather than repainting it or
// building a second host.
mkRecorded(19);
mkPatchBlock(19);
payload = { live: true, entries: [
    { key: 'patchset:5/patch:19/stage:goal', patch_id: 19, stage: 'goal',
      phase: { kind: 'stage_done', stage: 'goal', seconds: 574, turns: 8 },
      description: 'stage goal done in 9m 34s, 8 turns', age_seconds: 1212 },
    { key: 'patchset:5', patch_id: null, stage: null,
      phase: { kind: 'reviewing_patches', patches: 3 },
      description: 'reviewing 3 patches', age_seconds: 3563, idle_seconds: 20 },
]};
await ctx.refreshActivity(5);
check('a patch with a recorded table gets no second host',
    document.getElementById('stage-progress-19') === null);
check('and its stages do not leak into the patchset-wide row',
    !value.innerHTML.includes('stage goal'), value.innerHTML);
check('the patchset-wide entry is still reported',
    value.innerHTML.includes('reviewing 3 patches'));

// ---- findings table ------------------------------------------------------
// The severity rationale the review already writes, which the page discarded
// until now. Its shape comes from third_party/prompts/kernel/severity.md, so
// these cases are written against what that rubric actually produces.

const card = (findings, extra = {}) => ctx.renderReviewCard({
    id: 7, status: 'Reviewed', patch_id: 3,
    output: JSON.stringify({ findings }), ...extra,
});

const threeLabels = ctx.parseSeverityCalibration(
    'Consequence: a leaked page per call. Triggering path: any caller taking the '
    + 'error branch. Reachability: reachable by unprivileged ioctl.');
check('all three calibration labels are found',
    threeLabels.sections.length === 3, JSON.stringify(threeLabels));
check('calibration sections keep the rubric order',
    threeLabels.sections.map(s => s.label).join('|')
        === 'consequence|triggering path|reachability',
    JSON.stringify(threeLabels.sections.map(s => s.label)));
check('a section body stops at the next label',
    threeLabels.sections[0].body === 'a leaked page per call.',
    threeLabels.sections[0].body);

// The reason the label regex needs a leading boundary at all: the rubric's own
// vocabulary appears inside the prose it writes.
const inProse = ctx.parseSeverityCalibration(
    'Consequence: nothing, because reachability is speculative here.');
check('a label mentioned in prose does not open a section',
    inProse.sections.length === 1, JSON.stringify(inProse.sections));

// Every review recorded before the rubric asked for labels.
const unlabelled = ctx.parseSeverityCalibration('1. Condition Y is met.\n2. The buffer leaks.');
check('an unlabelled explanation survives as one block',
    unlabelled.sections.length === 0 && unlabelled.preamble.includes('The buffer leaks'),
    JSON.stringify(unlabelled));
check('an empty explanation is not a section',
    ctx.parseSeverityCalibration('   ') === null);

// A /g regex held at module scope would carry lastIndex into the next finding.
const repeated = [0, 1, 2].map(() =>
    ctx.parseSeverityCalibration('Consequence: x. Reachability: y.').sections.length);
check('parsing is not stateful across findings',
    repeated.every(n => n === 2), JSON.stringify(repeated));

check('a capped finding is marked speculative',
    ctx.isSpeculativeFinding('Consequence: unclear, so capped at Medium.', 'Medium'));
check('a High finding is never marked speculative',
    !ctx.isSpeculativeFinding('Not capped as speculative; the path is proven.', 'High'));
check('an explicit "not speculative" is not marked',
    !ctx.isSpeculativeFinding('Reachability is proven, so not speculative.', 'Low'));

check('a headline is used verbatim',
    ctx.findingHeadline({ headline: 'Admin queue logs at the wrong level.', problem: 'x'.repeat(400) })
        === 'Admin queue logs at the wrong level.');
const clipped = ctx.findingHeadline({ problem: 'y'.repeat(400) });
check('a finding with no headline falls back to a clipped problem',
    clipped.length < 200 && clipped.endsWith('…'), clipped);

const full = 'z'.repeat(400);
const fallbackCard = card([{ severity: 'Low', problem: full }]);
check('the untruncated problem is still reachable in the expansion',
    fallbackCard.includes(full), 'full problem text missing from detail row');

const table = card([
    { severity: 'Low', headline: 'Low one', problem: 'Low one', severity_explanation: 'Consequence: minor.' },
    { severity: 'Critical', headline: 'Critical one', problem: 'Critical one', severity_explanation: 'Consequence: heap corruption.' },
    { severity: 'Medium', headline: 'Preexisting one', problem: 'Preexisting one', preexisting: true },
]);
check('the findings table is rendered and counted',
    table.includes('Findings (3)'), table.slice(0, 200));
check('findings are listed severest first',
    table.indexOf('Critical one') < table.indexOf('Preexisting one')
        && table.indexOf('Preexisting one') < table.indexOf('Low one'));
check('a preexisting finding says so',
    table.includes('(preexisting)'));
check('the table starts collapsed',
    table.includes('class="collapsible"') && !table.includes('collapsible open'));

// A control that opens an empty box is worse than no control.
const bare = card([{ severity: 'Low', headline: 'Nothing behind this' }]);
check('a finding with no detail gets no toggle',
    !bare.includes('toggleFindingReasoning'), bare.slice(0, 400));

check('a review with no findings renders no table',
    !card([]).includes('Findings ('));
check('a review with unparseable output renders no table',
    !ctx.renderReviewCard({ id: 7, status: 'Reviewed', patch_id: 3, output: '{ not json' })
        .includes('Findings ('));

const nasty = card([{
    severity: 'High',
    headline: '<script>alert(1)</script>',
    problem: '<img src=x onerror=alert(2)>',
    severity_explanation: 'Consequence: <b>bold</b> trouble.',
    locations: [{ file: '<svg onload=alert(3)>', function_or_symbol: 'f', line: 12 }],
}]);
check('finding text is escaped',
    !nasty.includes('<script>alert(1)</script>')
        && !nasty.includes('<img src=x')
        && !nasty.includes('<svg onload')
        && nasty.includes('&lt;script&gt;'), nasty.slice(0, 400));
check('a location is shown as file:function:line',
    nasty.includes('f:12'), nasty.slice(0, 600));

console.log(failures ? `\n${failures} FAILURE(S)` : '\nALL CHECKS PASSED');
process.exit(failures ? 1 : 0);
