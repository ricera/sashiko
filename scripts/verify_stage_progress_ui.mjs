// Checks the web UI's review card, the per-patch stage table (live and
// recorded), how live activity is routed and polled, the live conversation log,
// and the line naming the reference repositories each review had.
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

// Extracts one module-level `let` declaration, which the page writes on a
// single line. Read from the page for the same reason the functions are: the
// polling state's initial shape is part of what is being checked.
function grabLet(name) {
    const m = new RegExp(`\\n\\s*(let ${name} = [^\\n]*;)`).exec(script);
    if (!m) throw new Error(`no single-line let ${name}`);
    return m[1];
}

const LETS = ['activityPoll', 'viewGeneration', 'logView', 'logTimer'];
const NAMES = ['escapeHtml', 'renderReviewReferences', 'renderReviewCard',
               'formatDuration', 'describeStageWait', 'summarizeReason', 'compareStageRows',
               'stageTableLabel', 'stageRowsFromRecord', 'stageRowsFromActivity',
               'renderStageRow', 'stageTableParts', 'hostForPatch', 'paintStageProgress',
               'refreshActivity', 'startActivityPolling', 'stopActivityPolling',
               'renderInteractionLogEntries', 'formatJsonExpandable', 'formatFuncCallExpandable',
               'getToolPreview', 'formatSize', 'formatJsonPretty', 'sortToolArgs',
               'renderLogView', 'appendNewLogEntries', 'lastSeqOf', 'liveLogRunOf', 'stopLogPolling'];

// Defines the named functions and lets in a scope of their own and returns
// them, so a test can build a variant with one function's source replaced and
// check that the suite notices. The lets are reached through `state()`, since
// the functions reassign them.
function load(sources) {
    const ctx = {};
    new Function('ctx',
        LETS.map(grabLet).join('\n') + '\n\n'
        + NAMES.map(n => sources[n]).join('\n\n')
        + `\n;` + NAMES.map(n => `ctx.${n} = ${n};`).join('')
        + `\nctx.state = () => ({ ${LETS.join(', ')} });`
        // What router() does to the view counter, for a check that navigates
        // while a redraw is out; a source check below holds router() to it.
        + `\nctx.navigate = () => { viewGeneration++; };`)(ctx);
    return ctx;
}
const sources = Object.fromEntries(NAMES.map(n => [n, grab(n)]));
const ctx = load(sources);

// ---- minimal DOM ---------------------------------------------------------
// Enough of one to exercise the routing: ids, a class/tag querySelector, the
// attribute selector for recorded tables, element creation (hosts are built on
// demand), and insertAdjacentHTML for the appended log.
class El {
    constructor(id, tag = 'div') {
        this.id = id; this.tag = tag; this.style = {}; this.attrs = new Map();
        this.children = []; this._html = ''; this.textContent = ''; this.title = '';
        this.className = ''; this.open = false;
    }
    get innerHTML() { return this._html; }
    set innerHTML(v) {
        this._html = v;
        // A browser keeps textContent in step with innerHTML.
        this.textContent = v.replace(/<[^>]*>/g, '');
        // A browser makes elements written as innerHTML reachable by id, and
        // replaces the ones it overwrote. The log page relies on that: it
        // writes #log-entries and appends to it afterwards.
        for (const m of v.matchAll(/id="([^"]+)"/g)) {
            byId.set(m[1], new El(m[1], 'span'));
        }
        // The log page's entries container is the last element it writes, so
        // its content is everything up to the closing tag at the end.
        const entries = /<div id="log-entries">([\s\S]*)<\/div>\s*$/.exec(v);
        if (entries) byId.get('log-entries').innerHTML = entries[1];
        // Only the shape hostForPatch builds; enough for paintStageProgress to
        // find its summary and body.
        if (v.includes('stage-progress-body')) {
            const summary = new El(null, 'summary');
            const body = new El(null, 'div'); body.cls = 'stage-progress-body';
            this.children = [summary, body];
        }
    }
    insertAdjacentHTML(where, html) {
        if (where !== 'beforeend') throw new Error(`unexpected ${where}`);
        this._html += html;
        this.textContent = this._html.replace(/<[^>]*>/g, '');
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
// one with no reviews yet.
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
const app = new El('app');
byId.set('activity-row', row); byId.set('activity-value', value); byId.set('app', app);

globalThis.document = {
    getElementById: id => byId.get(id) || null,
    querySelectorAll: sel => sel === '[data-stage-progress]' ? all : [],
    querySelector: sel => {
        const m = /^\[data-stage-recorded="(.+)"\]$/.exec(sel);
        return m ? (recorded.get(m[1]) || null) : null;
    },
    createElement: tag => new El(null, tag),
};
globalThis.config = {};
const scrolls = [];
globalThis.window = { scrollY: 0, scrollTo: (x, y) => { scrolls.push(y); } };

// Timers are recorded rather than run, so a check can count what is pending
// and fire one by hand.
const timers = new Map();
let nextTimer = 1;
globalThis.setTimeout = (fn, ms) => { const id = nextTimer++; timers.set(id, fn); return id; };
globalThis.clearTimeout = id => { timers.delete(id); };
const pending = () => timers.size;
async function fireAll() {
    const fns = [...timers.values()];
    timers.clear();
    for (const fn of fns) fn();
    await flush();
}
const flush = async () => { for (let i = 0; i < 10; i++) await new Promise(r => setImmediate(r)); };

// The redraw refreshActivity asks for once a patchset it saw running finishes.
// It may be held, so a check can navigate while its read is out.
const redraws = [];
let redrawGate = null;
globalThis.parseHash = () => ({ view: 'patchset', id: '7', query: { part: '2', page: '1' } });
globalThis.renderPatchsetView = async (id, query, view) => {
    redraws.push({ id, query, view });
    if (redrawGate) await redrawGate;
};

// Every request goes through `respond`, which a check replaces. It may return
// a promise, to hold a response back.
const requests = [];
let respond = () => ({});
globalThis.fetch = async url => {
    requests.push(url);
    const body = await respond(url);
    if (body && body.__status) return { ok: false, status: body.__status, json: async () => ({}) };
    return { ok: true, status: 200, json: async () => body };
};

let failures = 0;
const check = (name, cond, detail = '') => {
    if (cond) { console.log(`  ok   ${name}`); }
    else { failures++; console.log(`  FAIL ${name}${detail ? ' :: ' + detail : ''}`); }
};

// Reads activity as the poll would, for a poll that is current.
let payload;
respond = () => payload;
const refresh = id => ctx.refreshActivity(id, ctx.state().activityPoll.generation);

// Assertions are scoped to one row. Searching the whole table lets a sibling
// row's turn counter, or the tooltip's full untruncated reason, satisfy a check
// that the visible cell should have failed.
function rowFor(html, label) {
    const r = html.split('<tr').find(r => r.includes(`>${label}<`));
    if (!r) throw new Error(`no row for ${label}`);
    return '<tr' + r;
}
const visibleText = r => r.replace(/<[^>]*>/g, ' ');
const ORDER = { goal: 0, implementation: 1, 'execution-flow': 2, resources: 3, locking: 4,
                security: 5, hardware: 6, deduplication: 7, 'conflict-resolution': 8,
                verification: 9, report: 10 };

// ---- the review card -----------------------------------------------------
console.log('review card');
{
    const done = ctx.renderReviewCard({ id: 42, status: 'Reviewed', patch_id: 10 });
    check('a finished review links to its log', done.includes('#/log/42'), done.slice(0, 300));
    check('a finished review shows its token count', done.includes('Tokens used'));
    const noId = ctx.renderReviewCard({ status: 'Reviewed', patch_id: 10 });
    check('no link when there is no review to link to', !noId.includes('#/log/'), noId.slice(0, 300));
    const running = ctx.renderReviewCard({ id: 42, status: 'In Review', patch_id: 10 });
    check('a running review shows no token count', !running.includes('Tokens used'));
    check('empty findings read as no regressions',
        ctx.renderReviewCard({ id: 7, status: 'Reviewed', patch_id: 3,
            output: JSON.stringify({ findings: [] }) }).includes('No regressions'));
    let threw = null;
    try {
        ctx.renderReviewCard({ id: 7, status: 'Reviewed', patch_id: 3, output: '{ not json' });
    } catch (e) { threw = e; }
    check('unparseable output does not throw', threw === null, String(threw));
    const htmlResult = ctx.renderReviewCard({ id: 8, status: 'Reviewed', patch_id: 3,
        result: '<b>x</b>' });
    check('an HTML result is escaped',
        !htmlResult.includes('<b>x</b>') && htmlResult.includes('&lt;b&gt;x&lt;/b&gt;'),
        htmlResult.slice(0, 400));
}

// ---- case 1: live stages route to their own patch -------------------------
const h10 = mkHost(10), h11 = mkHost(11);
payload = { live: true, entries: [
    { key: 'patchset:7', phase: { kind: 'reviewing' }, description: 'running review stages',
      age_seconds: 100, idle_seconds: 5, patch_id: null, stage: null, order: null },
    // Deliberately after its sibling: the page must order by `order`.
    { key: 'patchset:7/patch:10/stage:locking', patch_id: 10, stage: 'locking', order: ORDER.locking,
      phase: { kind: 'stage', stage: 'locking', turn: 2, max_turns: 50,
               waiting: { on: 'tools', names: ['git_grep', 'git_show'] } },
      description: 'stage locking, turn 2/50 (running git_grep, git_show)',
      age_seconds: 60, idle_seconds: 400 },
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      order: ORDER['execution-flow'],
      phase: { kind: 'stage', stage: 'execution-flow', turn: 7, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage execution-flow, turn 7/50 (awaiting model)', age_seconds: 300, idle_seconds: 12 },
    { key: 'patchset:7/patch:99/stage:goal', patch_id: 99, stage: 'goal', order: ORDER.goal,
      phase: { kind: 'stage', stage: 'goal', turn: 1, max_turns: 50, waiting: { on: 'queued' } },
      description: 'stage goal, turn 1/50 (queued for a model slot)',
      age_seconds: 30, idle_seconds: 1 },
]};
await refresh(7);

console.log('case 1: live routing');
{
    const body = h10.querySelector('.stage-progress-body').innerHTML;
    check('patch 10 host shown', h10.style.display === '');
    check('patch 10 got both its stages',
        body.includes('Stage execution-flow') && body.includes('Stage locking'));
    check('stage rows follow the server order, not arrival order',
        body.indexOf('Stage execution-flow') < body.indexOf('Stage locking'));
    check('tool wait is spelled out', body.includes('running git_grep, git_show'));
    check('stalled stage is flagged', body.includes('no progress for 6m 40s'));
    check('turn counter shown', body.includes('turn 7/50'));
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
}

// ---- case 2: a stage that finished stops claiming to run ------------------
payload = { live: true, entries: [
    { key: 'patchset:7', phase: { kind: 'reviewing' }, description: 'running review stages',
      age_seconds: 400, idle_seconds: 2, patch_id: null, stage: null },
]};
await refresh(7);
console.log('case 2: stages finish');
check('host hidden once its stages are gone', h10.style.display === 'none');

// ---- case 3: nothing at all hides the top row ----------------------------
payload = { live: true, entries: [] };
await refresh(7);
console.log('case 3: idle');
check('top row hidden when there is nothing to say', row.style.display === 'none');

// ---- case 5: a failed stage keeps its row and stops claiming to run -------
payload = { live: true, entries: [
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      order: ORDER['execution-flow'],
      phase: { kind: 'stage', stage: 'execution-flow', turn: 7, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage execution-flow, turn 7/50 (awaiting model)', age_seconds: 300, idle_seconds: 12 },
    { key: 'patchset:7/patch:10/stage:implementation', patch_id: 10, stage: 'implementation',
      order: ORDER.implementation,
      phase: { kind: 'stage_failed', stage: 'implementation', cancelled: false,
               reason: 'Session exceeded max turns limit (50)\n\nCaused by:\n    nothing' },
      description: 'stage implementation failed: Session exceeded max turns limit (50)',
      age_seconds: 90, idle_seconds: 90 },
    { key: 'patchset:7/patch:10/stage:security', patch_id: 10, stage: 'security', order: ORDER.security,
      phase: { kind: 'stage_failed', stage: 'security', cancelled: true,
               reason: 'Session cancelled by supervisor' },
      description: 'stage security cancelled: Session cancelled by supervisor',
      age_seconds: 30, idle_seconds: 30 },
]};
await refresh(7);
console.log('case 5: failed stages');
{
    const body5 = h10.querySelector('.stage-progress-body').innerHTML;
    const failedRow = rowFor(body5, 'Stage implementation');
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
}

// ---- case 4: persisted (daemon stopped) ---------------------------------
payload = { live: false, entries: [
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      order: ORDER['execution-flow'],
      phase: { kind: 'stage' }, description: 'stage execution-flow, turn 7/50 (awaiting model)',
      updated_at: 1200 },
    { key: 'commit:abc', patch_id: null, stage: null, order: null,
      phase: { kind: 'fetching' }, description: 'fetching 2 commit(s) from origin',
      updated_at: 1200 },
]};
await refresh(7);
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
console.log('case 6: log link availability');
{
    for (const status of ['In Review', 'Pending']) {
        const card = ctx.renderReviewCard({ id: 42, status, patch_id: 10 });
        check(`link is present while ${status}`, card.includes('#/log/42'), card.slice(0, 300));
        check(`no token count while ${status}`, !card.includes('Tokens used'));
    }
    const done = ctx.renderReviewCard({ id: 42, status: 'Reviewed', patch_id: 10 });
    check('finished review still links to its log', done.includes('#/log/42'));
    const noId = ctx.renderReviewCard({ status: 'In Review', patch_id: 10 });
    check('no link when there is no review to link to', !noId.includes('#/log/'));
}

// ---- case 7: a patch whose card did not exist at render time -------------
console.log('case 7: hosts built on demand');
const block12 = mkPatchBlock(12);
payload = { live: true, entries: [
    { key: 'patchset:7', phase: { kind: 'reviewing_patches', patches: 2 },
      description: 'reviewing 2 patches', age_seconds: 900, idle_seconds: 3,
      patch_id: null, stage: null },
    { key: 'patchset:7/patch:12/stage:goal', patch_id: 12, stage: 'goal', order: ORDER.goal,
      phase: { kind: 'stage', stage: 'goal', turn: 3, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage goal, turn 3/50 (awaiting model)', age_seconds: 15, idle_seconds: 2 },
    { key: 'patchset:7/patch:12', patch_id: 12, stage: null, order: null,
      phase: { kind: 'planning', attempt: 1, max_attempts: 4 },
      description: 'planning stages', age_seconds: 20, idle_seconds: 20 },
]};
await refresh(7);
const built = document.getElementById('stage-progress-12');
const body7 = built ? built.querySelector('.stage-progress-body').innerHTML : '';
{
    check('a host is built for a patch that had no card', !!built);
    check('it is styled as a card, since it stands alone',
        !!built && built.classList.contains('review-card'));
    check('it sits directly under the patch title',
        !!built && block12.children.indexOf(built) === 1,
        built ? String(block12.children.indexOf(built)) : 'no host');
    check('the patch stages went there, not the top row', body7.includes('Stage goal'));
    check('the top row is left with the patchset only',
        value.innerHTML.includes('reviewing 2 patches')
            && !value.innerHTML.includes('stage goal, turn 3/50')
            && !value.innerHTML.includes('planning stages'),
        value.innerHTML);
}

// ---- case 8: the patch's own phase reads as the patch, not a stage --------
console.log('case 8: per-patch coarse entry');
{
    check('planning is labelled for the patch', body7.includes('>This patch<'));
    check('planning does not print a stage name', !body7.includes('Stage ?'));
    check('planning shows its own elapsed time', body7.includes('20s'));
    const summary7 = built ? built.querySelector('summary').textContent : 'no host';
    check('the patch entry is not counted as a running stage',
        summary7 === 'Stages (1 running)', summary7);
    check('the patch entry sorts above its stages, though it arrived after them',
        body7.indexOf('This patch') < body7.indexOf('Stage goal'));
}

// ---- case 9: finished stages stay on the card ----------------------------
console.log('case 9: completed stages');
// Deliberately not in stage order: the rows can only come out in review order
// if the page sorts them on `order`. A comparator that silently yields NaN --
// which is what subtracting two names does -- leaves the input order untouched
// and would pass a sorted fixture.
payload = { live: true, entries: [
    { key: 'patchset:7/patch:10/stage:resources', patch_id: 10, stage: 'resources', order: ORDER.resources,
      phase: { kind: 'stage_failed', stage: 'resources', cancelled: false, reason: 'boom' },
      description: 'stage resources failed: boom', age_seconds: 10, idle_seconds: 10 },
    { key: 'patchset:7/patch:10/stage:execution-flow', patch_id: 10, stage: 'execution-flow',
      order: ORDER['execution-flow'],
      phase: { kind: 'stage', stage: 'execution-flow', turn: 4, max_turns: 50, waiting: { on: 'model' } },
      description: 'stage execution-flow, turn 4/50 (awaiting model)', age_seconds: 60, idle_seconds: 2 },
    { key: 'patchset:7/patch:10/stage:goal', patch_id: 10, stage: 'goal', order: ORDER.goal,
      phase: { kind: 'stage_done', stage: 'goal', seconds: 185, turns: 12 },
      description: 'stage goal done in 3m 5s, 12 turns', age_seconds: 40, idle_seconds: 40 },
    { key: 'patchset:7/patch:10/stage:implementation', patch_id: 10, stage: 'implementation',
      order: ORDER.implementation,
      phase: { kind: 'stage_done', stage: 'implementation', seconds: 20, turns: 1 },
      description: 'stage implementation done in 20s, 1 turn', age_seconds: 5, idle_seconds: 5 },
]};
await refresh(7);
{
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
    check('rows come out in pipeline order',
        body9.indexOf('>Stage goal<') < body9.indexOf('>Stage implementation<')
            && body9.indexOf('>Stage implementation<') < body9.indexOf('>Stage execution-flow<')
            && body9.indexOf('>Stage execution-flow<') < body9.indexOf('>Stage resources<'));
}

// ---- case 10: the finished table's heading --------------------------------
console.log('case 10: stage table heading');
{
    const many = ctx.renderReviewCard({
        id: 7, status: 'Reviewed', patch_id: 10,
        stage_durations: [
            { stage: 'goal', order: ORDER.goal, seconds: 123, turns: 4 },
            { stage: 'implementation', order: ORDER.implementation, seconds: 20, turns: 1 },
            { stage: 'execution-flow', order: ORDER['execution-flow'], seconds: 617, turns: 9 },
        ],
    });
    check('the longest stage is named', many.includes('longest 10m 17s'), many.slice(0, 400));
    check('the sum is named, so the gap from the review time is visible',
        many.includes('12m 40s summed'));
    check('the count survives', many.includes('3 stages overlapping'));
    check('the old boilerplate is gone', !many.includes('run concurrently'));
    const one = ctx.renderReviewCard({
        id: 7, status: 'Reviewed', patch_id: 10,
        stage_durations: [{ stage: 'resources', order: ORDER.resources, seconds: 90, turns: 2 }],
    });
    check('a single stage just states its duration',
        one.includes('Stages (1 stage, 90s)'), one.slice(0, 400));
}

// ---- case 12: a failed review must not claim to have succeeded ------------
console.log('case 12: failed review cards');
{
    const failedCard = ctx.renderReviewCard({
        id: 9, status: 'Failed', patch_id: 10, attempt: 4, duration_seconds: 43200,
        result: 'Tool error: Review tool timed out (active time exceeded)',
        stage_durations: [
            { stage: 'goal', order: ORDER.goal, seconds: 120, turns: 3 },
            { stage: 'implementation', order: ORDER.implementation, seconds: 300, turns: 6 },
        ],
        stage_failures: [
            { stage: 'locking', order: ORDER.locking,
              reason: 'still running when the review stopped, after 2h 58m and 47 turn(s)', cancelled: true },
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
    check('the stage that did not finish is named',
        failedCard.includes('Stage locking') && failedCard.includes('2h 58m'),
        failedCard.slice(0, 600));
    check('an unfinished stage is not called a failure',
        visibleText(rowFor(failedCard, 'Stage locking')).includes('stopped:')
            && !visibleText(rowFor(failedCard, 'Stage locking')).includes('failed:'),
        failedCard.slice(0, 900));
    check('the banner does not repeat what the table lists',
        failedCard.split('Stage locking').length === 2, failedCard.slice(0, 900));
    check('the coverage gap is stated',
        failedCard.includes('did not complete'), failedCard.slice(0, 600));
    const cleanCard = ctx.renderReviewCard({
        id: 10, status: 'Reviewed', patch_id: 11, attempt: 1, duration_seconds: 90,
    });
    check('a first-attempt review still reads plainly',
        cleanCard.includes('Reviewed in 90s') && !cleanCard.includes('total')
            && !cleanCard.includes('attempt'), cleanCard.slice(0, 300));
    check('a retried success still says it succeeded',
        ctx.renderReviewCard({ id: 11, status: 'Reviewed', patch_id: 11, attempt: 2, duration_seconds: 90 })
            .includes('succeeded on attempt 2'));
}

// ---- case 13: one table, live and recorded -------------------------------
console.log('case 13: one stage table');
{
    // A finished stage must render identically whether its numbers arrived
    // from the registry a second ago or from the database an hour later.
    const fromRecord = ctx.stageRowsFromRecord(
        [{ stage: 'goal', order: 0, stage_label: null, seconds: 574, turns: 8 }], []);
    const fromActivity = ctx.stageRowsFromActivity([{
        key: 'patchset:5/patch:19/stage:goal', patch_id: 19, stage: 'goal', order: 0, stage_label: null,
        phase: { kind: 'stage_done', stage: 'goal', seconds: 574, turns: 8 },
        description: 'stage goal done in 9m 34s, 8 turns', age_seconds: 1212,
    }], true);
    check('a finished stage renders the same from either source',
        ctx.renderStageRow(fromRecord[0]) === ctx.renderStageRow(fromActivity[0]),
        `${ctx.renderStageRow(fromRecord[0])}\n!==\n${ctx.renderStageRow(fromActivity[0])}`);
    const labelled = r => ({ ...r, stage_label: 'Goal Analysis' });
    check('and with a label too',
        ctx.renderStageRow(labelled(fromRecord[0])) === ctx.renderStageRow(labelled(fromActivity[0])));
    check('and it is the row the reader wants',
        visibleText(ctx.renderStageRow(fromRecord[0])).includes('9m 34s')
            && visibleText(ctx.renderStageRow(fromRecord[0])).includes('8 turns'),
        ctx.renderStageRow(fromRecord[0]));
    const stoppedRow = ctx.stageRowsFromRecord([], [
        { stage: 'locking', order: 4, reason: 'still running when the review stopped, after 2h 58m', cancelled: true },
    ])[0];
    check('a recorded stopped stage does not invent a duration',
        visibleText(ctx.renderStageRow(stoppedRow)).includes('—'), ctx.renderStageRow(stoppedRow));
    check('durations and failures are one list, in stage order',
        ctx.stageRowsFromRecord(
            [{ stage: 'security', order: 5, seconds: 10, turns: 1 }],
            [{ stage: 'goal', order: 0, reason: 'x', cancelled: true }],
        ).map(r => r.stage).join(',') === 'goal,security');

    const recordedCard = ctx.renderReviewCard({
        id: 8, status: 'Reviewed', patch_id: 19,
        stage_durations: [{ stage: 'goal', order: 0, seconds: 574, turns: 8 }],
    });
    check('a recorded card marks itself as holding the stages',
        recordedCard.includes('data-stage-recorded="19"'), recordedCard.slice(0, 400));
    check('the recorded table carries a status column like the live one',
        visibleText(rowFor(recordedCard, 'Stage goal')).includes('done'),
        rowFor(recordedCard, 'Stage goal'));

    mkRecorded(19);
    mkPatchBlock(19);
    payload = { live: true, entries: [
        { key: 'patchset:5/patch:19/stage:goal', patch_id: 19, stage: 'goal', order: 0,
          phase: { kind: 'stage_done', stage: 'goal', seconds: 574, turns: 8 },
          description: 'stage goal done in 9m 34s, 8 turns', age_seconds: 1212 },
        { key: 'patchset:5', patch_id: null, stage: null,
          phase: { kind: 'reviewing_patches', patches: 3 },
          description: 'reviewing 3 patches', age_seconds: 3563, idle_seconds: 20 },
    ]};
    await refresh(5);
    check('a patch with a recorded table gets no second host',
        document.getElementById('stage-progress-19') === null);
    check('and its stages do not leak into the patchset-wide row',
        !value.innerHTML.includes('stage goal'), value.innerHTML);
    check('the patchset-wide entry is still reported',
        value.innerHTML.includes('reviewing 3 patches'));
}

// ---- stage order, labels and escaping -------------------------------------
// Checks a set of functions and returns the names of those that failed, so the
// same list can be run against broken copies and expected to fail there.
function stageChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    const guard = f => { try { return f(); } catch (e) { return `THREW ${e}`; } };

    // (a) out of pipeline order in, pipeline order out, from both sources
    const live = guard(() => fns.stageRowsFromActivity([
        { patch_id: 1, stage: 'report', order: 10, phase: { kind: 'stage_done', seconds: 1, turns: 1 } },
        { patch_id: 1, stage: 'goal', order: 0, phase: { kind: 'stage_done', seconds: 1, turns: 1 } },
        { patch_id: 1, stage: 'mystery', order: null, phase: { kind: 'stage_done', seconds: 1, turns: 1 } },
        { patch_id: 1, stage: 'locking', order: 4, phase: { kind: 'stage_done', seconds: 1, turns: 1 } },
        { patch_id: 1, stage: null, phase: { kind: 'planning' }, description: 'planning' },
    ], true));
    expect('order: live rows sorted by order, patch first, unknown last',
        Array.isArray(live) && live.map(r => r.stage).join(',') === ',goal,locking,report,mystery');
    const rec = guard(() => fns.stageRowsFromRecord(
        [{ stage: 'mystery', order: null, seconds: 1 }, { stage: 'report', order: 10, seconds: 1 },
         { stage: 'goal', order: 0, seconds: 1 }],
        [{ stage: 'locking', order: 4, reason: 'x' }]));
    expect('order: recorded rows sorted by order, unknown last',
        Array.isArray(rec) && rec.map(r => r.stage).join(',') === 'goal,locking,report,mystery');

    // (b) null last, nulls tie, never NaN
    const rows = [
        { stage: 'goal', state: 'done', order: 0 }, { stage: 'x', state: 'done', order: null },
        { stage: 'y', state: 'done' }, { stage: 'report', state: 'done', order: 10 },
        { stage: null, state: 'patch', order: null }, { stage: 'z', state: 'running', order: undefined },
    ];
    let finite = true;
    for (const a of rows) for (const b of rows) {
        const c = guard(() => fns.compareStageRows(a, b));
        if (typeof c !== 'number' || !Number.isFinite(c)) finite = false;
    }
    expect('order: the comparator is finite for every pair', finite);
    expect('order: two unknown stages compare equal',
        guard(() => fns.compareStageRows(rows[1], rows[2])) === 0);
    expect('order: an unknown stage sorts after a known one',
        guard(() => fns.compareStageRows(rows[1], rows[3])) > 0
        && guard(() => fns.compareStageRows(rows[3], rows[1])) < 0);
    expect('order: the patch row sorts first',
        guard(() => fns.compareStageRows(rows[4], rows[0])) < 0);

    // (c) the label comes from the server
    const withLabel = guard(() => fns.renderStageRow(
        { stage: 'goal', stage_label: 'Goal Analysis', state: 'done', seconds: 3, turns: 1 }));
    expect('label: stage_label is shown', String(withLabel).includes('>Goal Analysis<'));
    // A persisted entry for the patch itself has no stage; it is the patch's
    // row, recorded, and must not read as an unknown stage.
    const persistedPatch = guard(() => fns.stageRowsFromActivity(
        [{ patch_id: 1, stage: null, order: null, description: 'reviewing patch 1' },
         { patch_id: 1, stage: 'goal', order: 0, description: 'stage goal, turn 2/50' }], false));
    const persistedHtml = Array.isArray(persistedPatch)
        ? persistedPatch.map(r => guard(() => fns.renderStageRow(r))).join('\n') : String(persistedPatch);
    expect('label: a persisted patch row reads "This patch"',
        persistedHtml.includes('>This patch<') && !persistedHtml.includes('Stage ?')
        && Array.isArray(persistedPatch) && persistedPatch[0].stage == null);
    expect('label: the name falls back when there is no label',
        String(guard(() => fns.renderStageRow({ stage: 'goal', state: 'done', seconds: 3 })))
            .includes('>Stage goal<'));

    // (d) every value is escaped, the reason in title= included
    const evil = '<script>x</script>"onerror=alert(1)\'';
    const nasty = [
        guard(() => fns.renderStageRow({ stage: evil, state: 'running', description: evil,
            waiting: { on: 'tools', names: [evil] }, max_turns: 3, turn: 1 })),
        guard(() => fns.renderStageRow({ stage: evil, stage_label: evil, state: 'stopped',
            reason: evil, cancelled: false })),
        guard(() => fns.renderStageRow({ stage: evil, state: 'persisted', description: evil })),
        guard(() => fns.renderStageRow({ stage: null, state: 'patch', description: evil })),
        guard(() => fns.renderStageRow({ stage: evil, state: 'done', seconds: 1, turns: 1 })),
    ].join('\n');
    expect('escaping: no raw markup or attribute break-out',
        !nasty.includes('<script>') && !nasty.includes('"onerror') && !nasty.includes('THREW'));
    expect('escaping: shown escaped', nasty.includes('&lt;script&gt;') && nasty.includes('&quot;onerror'));
    return failed;
}
console.log('stage order, labels and escaping');
{
    const stageFailures = stageChecks(ctx);
    check('every stage check passes', stageFailures.length === 0, stageFailures.join(', '));
}

// ---- polling hygiene -------------------------------------------------------
console.log('activity polling');
{
    timers.clear();
    redraws.length = 0;
    ctx.stopActivityPolling();
    payload = { live: true, finished: false, entries: [] };

    // (e) one chain, however often it is started
    ctx.startActivityPolling(7);
    ctx.startActivityPolling(7);
    await flush();
    check('starting twice leaves exactly one pending read', pending() === 1, String(pending()));
    await fireAll();
    check('each read schedules exactly one more', pending() === 1, String(pending()));
    ctx.stopActivityPolling();
    check('stopping leaves nothing pending', pending() === 0, String(pending()));

    // A patchset already finished gets one read and no redraw.
    payload = { live: false, finished: true, entries: [] };
    ctx.startActivityPolling(7);
    await flush();
    check('a finished patchset schedules nothing further', pending() === 0, String(pending()));
    check('and is not redrawn, since this page never saw it running', redraws.length === 0);

    // One seen running and then finished is redrawn, once, without the
    // part/patch keys that would scroll it again.
    payload = { live: true, finished: false, entries: [] };
    ctx.startActivityPolling(7);
    await flush();
    payload = { live: true, finished: true, entries: [] };
    await fireAll();
    check('finishing stops the poll', pending() === 0, String(pending()));
    check('a page that saw it running is redrawn once', redraws.length === 1, JSON.stringify(redraws));
    check('the redraw does not scroll to a part again',
        redraws.length === 1 && redraws[0].query.part === undefined && redraws[0].query.page === '1');
    check('the redraw is tied to the view on screen',
        redraws.length === 1 && redraws[0].view === ctx.state().viewGeneration, JSON.stringify(redraws));

    // Navigating away while the redraw's read is out cancels what it would
    // still do. renderPatchsetView checks its view itself (source check below);
    // the scroll restore is refreshActivity's own.
    redraws.length = 0; scrolls.length = 0;
    let openGate;
    redrawGate = new Promise(r => { openGate = r; });
    payload = { live: true, finished: false, entries: [] };
    ctx.startActivityPolling(7);
    await flush();
    payload = { live: true, finished: true, entries: [] };
    await fireAll();
    const asked = ctx.state().viewGeneration;
    ctx.navigate();
    openGate(); redrawGate = null;
    await flush();
    check('a redraw overtaken by navigation was asked for the old view',
        redraws.length === 1 && redraws[0].view === asked, JSON.stringify(redraws));
    check('and does not scroll the view that replaced it', scrolls.length === 0, JSON.stringify(scrolls));

    // The redraw's paint is guarded inside renderPatchsetView, after its read
    // and before it touches the page; router() is what moves the counter.
    const psv = grab('renderPatchsetView');
    const afterRead = psv.indexOf('await res.json()');
    const guardAt = psv.indexOf('if (view !== viewGeneration) return;');
    check('renderPatchsetView takes the view it was asked for',
        /^async function renderPatchsetView\(id, query = \{\}, view = viewGeneration\)/.test(psv),
        psv.slice(0, 120));
    check('and drops its paint when another view was drawn during its read',
        afterRead > 0 && guardAt > afterRead && guardAt < psv.indexOf('state.patchsetId =')
        && guardAt < psv.indexOf('innerHTML ='));
    check('router() moves the view counter', /viewGeneration\+\+/.test(grab('router')));

    // (f) a response for a stopped poll does not paint
    row.style.display = 'none'; value.innerHTML = '';
    let release;
    respond = () => new Promise(r => { release = r; });
    ctx.startActivityPolling(7);
    await flush();
    ctx.stopActivityPolling();
    release({ live: true, finished: false, entries: [
        { key: 'patchset:7', patch_id: null, stage: null, description: 'stale news', age_seconds: 1 }] });
    await flush();
    check('a stale response does not paint', !value.innerHTML.includes('stale news')
        && row.style.display === 'none', value.innerHTML);
    check('and schedules nothing', pending() === 0, String(pending()));
    respond = () => payload;
}

// ---- the log ---------------------------------------------------------------
const MESSAGES = [
    { role: 'system', content: 'You are a reviewer.' },
    { role: 'user', content: 'Review <this> patch & tell me' },
    { role: 'assistant', content: 'Looking.', tool_calls: [
        { id: 'c0', function_name: 'git_grep', arguments: { pattern: 'spin_lock', path: 'drivers' } }] },
    { role: 'tool', tool_call_id: 'c0', content: JSON.stringify({ content: 'a.c:1: spin_lock(&l);\n' }) },
    { role: 'assistant', content: '{"findings": []}' },
];
const withSeq = (msgs, from = 0) => msgs.map((m, i) => ({ ...m, seq: from + i,
    stage: from + i ? 'goal' : null, stage_label: from + i ? 'Goal Analysis' : null, created_at: 1000 + i }));
const entriesHtml = () => {
    const el = byId.get('log-entries');
    return el ? el.innerHTML : null;
};

// Renders `review` as the log page, returning the #log-entries markup.
async function logPage(fns, review) {
    respond = () => review;
    await fns.renderLogView(42);
    return entriesHtml();
}

// (g) one renderer for both logs. Returns failed check names, so it can be run
// against a broken copy.
async function logChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    timers.clear();
    let finished, live;
    try {
        finished = await logPage(fns, { id: 42, status: 'Reviewed', logs: JSON.stringify(MESSAGES) });
        const finishedApp = app.innerHTML;
        live = await logPage(fns, { id: 42, status: 'In Review', logs: null, live_log: withSeq(MESSAGES) });
        expect('log: the finished page has no running banner', !finishedApp.includes('still running'));
    } catch (e) {
        failed.push(`log: threw ${e}`);
        return failed;
    }
    expect('log: the running page says so', app.innerHTML.includes('still running'));
    // A live entry adds only its stage mark; everything else is the same bytes.
    const unmarked = live === null ? null : live.replace(/<div class="log-stage"[^>]*>[^<]*<\/div>/g, '');
    expect('log: live and finished entries are byte-identical but for stage marks',
        finished !== null && finished === unmarked);
    expect('log: every staged live entry is marked, the system message is not',
        live !== null && live.split('class="log-stage"').length - 1 === MESSAGES.length - 1);
    expect('log: the server label is shown, the name kept as its title',
        live !== null && live.includes('title="goal"') && live.includes('>Goal Analysis<'));
    let stageless = null;
    try {
        stageless = await logPage(fns, { id: 42, status: 'In Review', logs: null,
            live_log: withSeq(MESSAGES).map(({ stage, stage_label, ...e }) => e) });
    } catch (e) { failed.push(`log: threw ${e}`); }
    expect('log: a live log without stages is byte-identical to the finished one',
        stageless !== null && stageless === finished);
    let evilStage = null;
    try {
        const evil = '<script>x</script>"onerror=alert(1)';
        evilStage = await logPage(fns, { id: 42, status: 'In Review', logs: null,
            live_log: [{ role: 'user', content: 'hi', seq: 0, stage: evil, stage_label: null }] });
    } catch (e) { failed.push(`log: threw ${e}`); }
    expect('log: a stage name is escaped in text and title',
        evilStage !== null && !evilStage.includes('<script>') && !evilStage.includes('"onerror')
        && evilStage.includes('&lt;script&gt;'));
    expect('log: tool calls are expandable calls, not prose',
        live !== null && live.includes('fn-name') && live.includes('git_grep'));
    expect('log: content is escaped', live !== null && live.includes('Review &lt;this&gt; patch &amp;'));
    fns.stopLogPolling();
    timers.clear();
    return failed;
}
console.log('live log');
{
    const logFailures = await logChecks(ctx);
    check('every log check passes', logFailures.length === 0, logFailures.join(', '));

    // An empty live log is still live, and says so rather than polling silently.
    timers.clear();
    await logPage(ctx, { id: 42, status: 'Pending', logs: null, live_log: [] });
    check('a review with nothing streamed yet still polls', pending() === 1, String(pending()));
    check('and shows the empty placeholder', (entriesHtml() || '').includes('No logs recorded'));
    ctx.stopLogPolling();
    check('stopping the log poll leaves nothing pending', pending() === 0);

    // A review that ended without a final log is not polled forever.
    timers.clear();
    await logPage(ctx, { id: 42, status: 'Failed', logs: null });
    check('an ended review without a log is not polled', pending() === 0, String(pending()));

    // The status is data and is escaped.
    await logPage(ctx, { id: 42, status: '<img src=x onerror=alert(1)>', logs: '[]' });
    check('the status in the header is escaped', !app.innerHTML.includes('<img'), app.innerHTML.slice(0, 600));

    // (h) appends only what is new, once
    timers.clear();
    requests.length = 0;
    await logPage(ctx, { id: 42, status: 'In Review', logs: null, live_log: withSeq(MESSAGES.slice(0, 2)) });
    check('the first page holds what was streamed', ctx.state().logView.lastSeq === 1);
    check('one poll is pending', pending() === 1, String(pending()));
    const { generation } = ctx.state().logView;
    let hold;
    respond = url => url.includes('after_seq') ? new Promise(r => { hold = r; }) : null;
    const a = ctx.appendNewLogEntries(42, generation);
    const b = ctx.appendNewLogEntries(42, generation);
    await flush();
    check('two overlapping polls make one request',
        requests.filter(u => u.includes('after_seq')).length === 1, JSON.stringify(requests));
    check('the request asks only for what follows', requests.some(u => u.endsWith('after_seq=1')),
        JSON.stringify(requests));
    // The server overlaps what the page has: entry 1 again, then 2 and 3.
    hold({ id: 42, status: 'In Review', logs: null, live_log: withSeq(MESSAGES.slice(1, 4), 1) });
    await a; await b; await flush();
    const html = entriesHtml() || '';
    check('an entry already on the page is not added again',
        html.split('Review &lt;this&gt; patch').length === 2, html);
    check('the new entries are appended', html.includes('git_grep') && html.includes('spin_lock'));
    check('the appended batch has its own ids', html.includes('json-call-s2-0'), html);
    check('lastSeq follows', ctx.state().logView.lastSeq === 3);
    check('exactly one next poll is pending', pending() === 1, String(pending()));

    // The finished log lands: the page is drawn again, wholesale, and stops.
    respond = () => ({ id: 42, status: 'Reviewed', logs: JSON.stringify(MESSAGES) });
    await fireAll();
    check('the finished log replaces the streamed one',
        !app.innerHTML.includes('still running') && (entriesHtml() || '').includes('{&quot;findings&quot;'),
        app.innerHTML.slice(0, 400));
    check('and nothing polls any more', pending() === 0, String(pending()));

    // (j) the conversation starts over. A restart reset the review and its
    // new run numbers entries past the old run's; the page holding the old
    // run's entries must draw the new run from the start, not splice it on.
    timers.clear();
    requests.length = 0;
    const OLD = [{ role: 'system', content: 'old system' }, { role: 'user', content: 'old run asks' }];
    const NEW = [{ role: 'system', content: 'new system' }, { role: 'user', content: 'new run asks' },
        { role: 'assistant', content: 'new run answers' }];
    await logPage(ctx, { id: 42, status: 'In Review', logs: null,
        live_log: withSeq(OLD), live_log_first_seq: 0 });
    check('the page records the run it shows', ctx.state().logView.firstSeq === 0
        && ctx.state().logView.lastSeq === 1, JSON.stringify(ctx.state().logView));
    // The server holds seqs 2..4 of the new run; after_seq=1 hides nothing of
    // it here, but a run that restarted at 0 would have been hidden entirely.
    respond = url => ({ id: 42, status: 'In Review', logs: null,
        live_log: withSeq(NEW, 2).filter(e => !url.includes('after_seq=1') || e.seq > 1),
        live_log_first_seq: 2 });
    await fireAll();
    const rerun = entriesHtml() || '';
    check('a new run replaces the old conversation rather than joining it',
        !rerun.includes('old run asks') && !rerun.includes('old system')
        && rerun.includes('new system') && rerun.includes('new run answers'), rerun);
    check('it is drawn from the start, with a full read',
        requests.some(u => u.endsWith('/api/review?id=42')), JSON.stringify(requests));
    check('and the page now follows the new run', ctx.state().logView.firstSeq === 2
        && ctx.state().logView.lastSeq === 4, JSON.stringify(ctx.state().logView));
    check('one poll is pending after the redraw', pending() === 1, String(pending()));

    // Between the new run clearing the old entries and writing its first,
    // the daemon holds none: the stale conversation goes.
    respond = () => ({ id: 42, status: 'In Review', logs: null, live_log: [], live_log_first_seq: null });
    await fireAll();
    check('entries cleared by a new run leave the page',
        !(entriesHtml() || '').includes('new run answers')
        && (entriesHtml() || '').includes('No logs recorded'));
    // An empty page has nothing to splice, so the first entries just append.
    requests.length = 0;
    respond = () => ({ id: 42, status: 'In Review', logs: null,
        live_log: withSeq(NEW.slice(0, 1), 5), live_log_first_seq: 5 });
    await fireAll();
    check('the first entries of a run are appended to an empty page',
        (entriesHtml() || '').includes('new system')
        && !requests.some(u => u.endsWith('/api/review?id=42')), JSON.stringify(requests));
    check('and the page adopts that run', ctx.state().logView.firstSeq === 5);
    // Same run, more entries: appended, no redraw.
    requests.length = 0;
    respond = () => ({ id: 42, status: 'In Review', logs: null,
        live_log: withSeq(NEW.slice(1, 2), 6), live_log_first_seq: 5 });
    await fireAll();
    check('the same run keeps appending',
        (entriesHtml() || '').includes('new system') && (entriesHtml() || '').includes('new run asks')
        && !requests.some(u => u.endsWith('/api/review?id=42')), JSON.stringify(requests));
    ctx.stopLogPolling();
    timers.clear();

    // A poll for a view that has been replaced does nothing.
    timers.clear();
    await logPage(ctx, { id: 42, status: 'In Review', logs: null, live_log: withSeq(MESSAGES.slice(0, 1)) });
    const stale = ctx.state().logView.generation;
    ctx.stopLogPolling();
    requests.length = 0;
    await ctx.appendNewLogEntries(42, stale);
    check('a stopped log poll makes no request', requests.length === 0 && pending() === 0);
    check('lastSeqOf ignores what is not a number',
        ctx.lastSeqOf([{ seq: '7' }, { seq: 2 }, {}, null, { seq: NaN }]) === 2
        && ctx.lastSeqOf([]) === -1 && ctx.lastSeqOf(undefined) === -1);
    respond = () => payload;
}

// (i) one link, one name
console.log('log link naming');
{
    for (const status of ['In Review', 'Pending', 'Reviewed', 'Failed']) {
        const card = ctx.renderReviewCard({ id: 42, status, patch_id: 10 });
        check(`"View Raw Log" while ${status}`, card.includes('>View Raw Log<')
            && !card.includes('>View Log<') && !card.includes('View Live Log'), card.slice(0, 300));
    }
}

// ---- reference repositories ------------------------------------------------
const SHA = '1a2b3c4d5e6f7a8b9c0d1a2b3c4d5e6f7a8b9c0d';
const RDMA_SHA = '9f8e7d6c5b4a39281706f5e4d3c2b1a098765432';
const finishedRefs = {
    attached: [
        { name: 'kernel', kind: 'kernel', revision: 'v6.18', commit: SHA, why: 'always', reads: 34 },
        { name: 'rdma-core', kind: 'source', revision: 'origin/ionic-rdma', commit: RDMA_SHA,
          why: 'match', file: 'rdma/drv/ionic/ionic_fw.h', glob: 'rdma/drv/**', reads: 1 },
    ],
    dormant: [{ name: 'iproute2', applies_to: ['ip/**'] }],
};

// Every reference check, against one set of functions. Returns the names of
// the checks that failed instead of reporting them, so the same list can be
// run against broken copies of the renderer and expected to fail there.
function referenceChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    const card = r => fns.renderReviewCard(r);
    const render = refs => {
        try { return fns.renderReviewReferences(refs); } catch (e) { return `THREW ${e}`; }
    };

    // present
    const line = render(finishedRefs);
    expect('present: labelled', line.includes('References:'));
    expect('present: name and revision', line.includes('kernel @ v6.18'));
    expect('present: short commit', line.includes(`(${SHA.slice(0, 12)})`)
        && line.includes(`(${RDMA_SHA.slice(0, 12)})`));
    expect('present: read count', line.includes('34 reads'));
    expect('present: one read is singular', line.includes('1 read') && !line.includes('1 reads'));
    expect('present: dormant note', line.includes('Not attached: iproute2')
        && line.includes('ip/**'));
    expect('present: shown on the card', card({ id: 1, status: 'Reviewed',
        reference_repos: finishedRefs }).includes('kernel @ v6.18'));
    expect('present: shown while running', card({ id: 1, status: 'In Review',
        reference_repos: finishedRefs }).includes('References:'));
    expect('present: a JSON string is accepted',
        render(JSON.stringify(finishedRefs)).includes('kernel @ v6.18'));

    // title
    expect('title: why it was attached',
        line.includes('attached because rdma/drv/ionic/ionic_fw.h matches rdma/drv/**')
        && line.includes('attached to every patch'));
    expect('title: full commit', line.includes(`commit ${SHA}`));

    // absent
    const plain = card({ id: 1, status: 'Reviewed' });
    expect('absent: no line', !plain.includes('References:'));
    expect('absent: null renders like missing',
        plain === card({ id: 1, status: 'Reviewed', reference_repos: null }));
    expect('absent: empty lists render nothing', render({ attached: [], dormant: [] }) === '');

    // missing reads: the review did not finish
    const early = render({ attached: [{ name: 'kernel', kind: 'kernel', revision: 'v6.18',
        commit: SHA, why: 'always' }] });
    expect('missing reads: no count', early.includes('kernel @ v6.18') && !/\bread/.test(
        early.replace(/title="[^"]*"/g, '')));
    expect('missing reads: title says not reported', early.includes('read count not reported'));

    // escaping
    const evil = '<script>x</script>"onerror=alert(1)';
    const nasty = render({
        attached: [{ name: evil, kind: 'source', revision: evil, commit: SHA, why: 'match',
            file: evil, glob: evil, reads: 2 }],
        dormant: [{ name: evil, applies_to: [evil] }],
    });
    expect('escaping: no raw markup', !nasty.includes('<script>') && !nasty.includes('"onerror'));
    expect('escaping: shown escaped', nasty.includes('&lt;script&gt;') && nasty.includes('&quot;'));

    // malformed
    expect('malformed: not a list', render({ attached: 'x' }) === '');
    expect('malformed: bad JSON', render('{bad json') === '');
    expect('malformed: not an object', render(42) === '' && render(undefined) === '');
    const noMatchFields = render({ attached: [{ name: 'rdma-core', kind: 'source',
        revision: 'v1', commit: SHA, why: 'match', reads: 1 }] });
    expect('malformed: a match without file and glob names neither',
        noMatchFields.includes('rdma-core @ v1') && !noMatchFields.includes('undefined')
        && !noMatchFields.includes('matches'));
    const noWhy = render({ attached: [{ name: 'kernel', kind: 'kernel', revision: 'v6.18',
        commit: SHA, reads: 1 }] });
    expect('malformed: no reason does not claim every patch',
        noWhy.includes('kernel @ v6.18') && !noWhy.includes('every patch'));
    const noGlobs = render({ attached: [], dormant: [{ name: 'rdma-core' }] });
    expect('malformed: a dormant reference without applies_to',
        noGlobs.includes('Not attached: rdma-core') && !noGlobs.includes('applies only to'));
    return failed;
}

console.log('reference repositories');
const refFailures = referenceChecks(ctx);
check('every reference check passes', refFailures.length === 0, refFailures.join(', '));

// ---- the checks catch what they are for -----------------------------------
// The checks above are only worth something if they fail on a broken page.
// Build broken copies from the page's own source and make sure they are caught.
console.log('mutations');
const replaceBody = (name, body) => {
    const fn = sources[name];
    const open = fn.indexOf('{');
    return fn.slice(0, open + 1) + body + '\n        }';
};
const mutate = (name, from, to) => {
    if (!sources[name].includes(from)) throw new Error(`mutation target missing in ${name}: ${from}`);
    return load({ ...sources, [name]: sources[name].replace(from, to) });
};
{
    const unescaped = load({ ...sources,
        renderReviewReferences: sources.renderReviewReferences.replaceAll('escapeHtml(', 'String(') });
    const empty = load({ ...sources,
        renderReviewReferences: replaceBody('renderReviewReferences', " return '';") });
    const unwired = mutate('renderReviewCard', '${referencesHtml}', '');
    check('a renderer that does not escape is caught', referenceChecks(unescaped).length > 0);
    check('a renderer that returns nothing is caught', referenceChecks(empty).length > 0);
    check('a card that drops the line is caught', referenceChecks(unwired).length > 0);

    const rowUnescaped = load({ ...sources,
        renderStageRow: sources.renderStageRow.replaceAll('escapeHtml(', 'String(') });
    check('a stage row that does not escape is caught', stageChecks(rowUnescaped).length > 0);
    const byName = load({ ...sources,
        compareStageRows: replaceBody('compareStageRows', ' return (a.stage || 0) - (b.stage || 0);') });
    check('a comparator subtracting stage names is caught', stageChecks(byName).length > 0);
    const byInfinity = load({ ...sources, compareStageRows: replaceBody('compareStageRows',
        ' return (a.order ?? Infinity) - (b.order ?? Infinity);') });
    check('a comparator subtracting missing ranks is caught', stageChecks(byInfinity).length > 0);

    const ownRenderer = mutate('renderLogView', '${renderInteractionLogEntries(logs)}',
        "${isLive ? logs.map(e => '<div>' + escapeHtml(e.content || '') + '</div>').join('')"
        + " : renderInteractionLogEntries(logs)}");
    check('a live log drawn by its own renderer is caught', (await logChecks(ownRenderer)).length > 0);
    ownRenderer.stopLogPolling();
}

console.log(failures ? `\n${failures} FAILURE(S)` : '\nALL CHECKS PASSED');
process.exit(failures ? 1 : 0);
