// Checks the web UI's review card and the findings it lists, the per-patch
// stage table (live and recorded), how live activity is routed and polled, the
// live conversation log, the line naming the reference repositories each review
// had, and how partial and timed-out reviews read in the list, the summary
// table, the card and the statistics.
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

// Extracts one module-level `const` declaration written on a single line. Read
// from the page so a limit such as the finding title length is checked as
// shipped rather than as a copy here remembers it.
function grabConst(name) {
    const m = new RegExp(`\\n\\s*(const ${name} = [^\\n]*;)`).exec(script);
    if (!m) throw new Error(`no single-line const ${name}`);
    return m[1];
}

const LETS = ['activityPoll', 'viewGeneration', 'logView', 'logTimer'];
const CONSTS = ['FINDING_TITLE_MAX_CHARS'];
const NAMES = ['escapeHtml', 'renderReviewReferences', 'renderReviewCard', 'getSeverityColor',
               'findingObject', 'findingTitle', 'parseSeverityCalibration', 'renderSeverityCalibration',
               'renderFindingLocations', 'findingTags', 'renderFindingRow', 'renderFindingsList',
               'formatDuration', 'describeStageWait', 'summarizeReason', 'compareStageRows',
               'stageTableLabel', 'stageRowsFromRecord', 'stageStopCause', 'stageStopLabel',
               'stoppedStageStatus', 'stageRowsFromActivity',
               'renderFindingsBadges', 'renderPatchsetListStatus', 'countNewFindings',
               'maxFindingCounts', 'maxOfRunsNote', 'patchSummaryStatus', 'renderStatCard', 'renderReviewStatCards',
               'reviewOutcomeColor',
               'isCallCount', 'toolCallBreakdown', 'toolCallsText', 'toolCallsTitle', 'reviewToolCallsValue',
               'toolCallsByPatch',
               'renderStageRow', 'stageTableParts', 'hostForPatch', 'paintStageProgress',
               'refreshActivity', 'startActivityPolling', 'stopActivityPolling',
               'renderInteractionLogEntries', 'formatJsonExpandable', 'formatFuncCallExpandable',
               'getToolPreview', 'formatSize', 'formatJsonPretty', 'sortToolArgs',
               'renderLogView', 'appendNewLogEntries', 'lastSeqOf', 'liveLogRunOf', 'stopLogPolling'];

// Defines the named functions, consts and lets in a scope of their own and
// returns them, so a test can build a variant with one function's source
// replaced and check that the suite notices. The lets are reached through
// `state()`, since the functions reassign them.
function load(sources) {
    const ctx = {};
    new Function('ctx',
        CONSTS.map(grabConst).join('\n') + '\n'
        + LETS.map(grabLet).join('\n') + '\n\n'
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
    body: {},
    getElementById: id => byId.get(id) || null,
    querySelectorAll: sel => sel === '[data-stage-progress]' ? all : [],
    querySelector: sel => {
        const m = /^\[data-stage-recorded="(.+)"\]$/.exec(sel);
        return m ? (recorded.get(m[1]) || null) : null;
    },
    createElement: tag => new El(null, tag),
};
globalThis.config = {};
// The theme's colours, as the stats chart reads them from the page.
const THEME = { '--status-inreview': ' #d67f05', '--status-partial': ' #9a6700' };
globalThis.getComputedStyle = () => ({ getPropertyValue: name => THEME[name] || '' });
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
      phase: { kind: 'stage_failed', stage: 'implementation', cause: 'error',
               reason: 'Session exceeded max turns limit (50)\n\nCaused by:\n    nothing' },
      description: 'stage implementation failed: Session exceeded max turns limit (50)',
      age_seconds: 90, idle_seconds: 90 },
    { key: 'patchset:7/patch:10/stage:security', patch_id: 10, stage: 'security', order: ORDER.security,
      phase: { kind: 'stage_failed', stage: 'security', cause: 'cancelled',
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
        visibleText(rowFor(body5, 'Stage security')).includes('cancelled: Session cancelled by supervisor')
            && !visibleText(rowFor(body5, 'Stage security')).includes('failed'));
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
      phase: { kind: 'stage_failed', stage: 'resources', cause: 'error', reason: 'boom' },
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

// ---- case 11: tool calls -----------------------------------------------------
// How much looking a review did, and where. Every attached reference is named
// once the review has made a call, a zero included, because "0 to kernel" on a
// review that looked elsewhere is the reading the count exists for. With no
// call, or no record of which references the review had, there is no clause.
// A card counts its own attempt and says so on a retry. The live stage table
// counts the patch's current run, every attempt of it, and says how many.
const KERNEL_AND_RDMA = { total: 350, by_reference: { references: [
    { name: 'kernel', calls: 72 }, { name: 'rdma-core', calls: 0 }], unattached: 0 } };
const split = (references, unattached = 0) => ({ references, unattached });

function toolCallWordingChecks(fns, expect) {
    const text = (counts, attempts) => fns.toolCallsText(counts, attempts);
    expect('wording: one call is singular', text({ total: 1 }) === '1 tool call');
    expect('wording: many calls are plural', text({ total: 350 }) === '350 tool calls');
    expect('wording: every reference in order, zeros included',
        text(KERNEL_AND_RDMA) === '350 tool calls (72 to kernel, 0 to rdma-core)');
    const kernel = [{ name: 'kernel', calls: 72 }];
    expect('wording: calls to unattached repositories are added',
        fns.toolCallBreakdown({ total: 80, by_reference: split(kernel, 3) })
            === ' (72 to kernel; 3 to unattached repositories)');
    expect('wording: one call to an unattached repository is singular',
        fns.toolCallBreakdown({ total: 80, by_reference: split(kernel, 1) })
            === ' (72 to kernel; 1 to an unattached repository)');
    expect('wording: no unattached clause at zero',
        fns.toolCallBreakdown({ total: 80, by_reference: split(kernel, 0) }) === ' (72 to kernel)');
    expect('wording: unattached calls with nothing attached',
        fns.toolCallBreakdown({ total: 3, by_reference: split([], 3) }) === ' (3 to unattached repositories)');
    expect('wording: nothing attached and nothing unattached gives no clause',
        fns.toolCallBreakdown({ total: 3, by_reference: split([], 0) }) === '');
    expect('wording: no call gives no clause, references or not',
        text({ total: 0, by_reference: split([{ name: 'kernel', calls: 0 }]) }) === '0 tool calls');
    expect('wording: an unknown split gives the total alone',
        text({ total: 350, by_reference: null }) === '350 tool calls'
        && text({ total: 350 }) === '350 tool calls');
    expect('wording: a run of several attempts says so',
        text({ total: 47 }, 3) === '47 tool calls over 3 attempts');
    expect('wording: a run of one attempt does not',
        text({ total: 47 }, 1) === '47 tool calls' && text({ total: 47 }) === '47 tool calls');
    const evil = text({ total: 2, by_reference: split([{ name: '<script>x</script>', calls: 2 }]) });
    expect('wording: a reference name is escaped',
        !evil.includes('<script>') && evil.includes('2 to &lt;script&gt;x&lt;/script&gt;'));
    const clauses = [text(KERNEL_AND_RDMA), fns.toolCallBreakdown({ total: 3, by_reference: split([], 3) }),
                     fns.toolCallBreakdown({ total: 3, by_reference: split([], 1) }), fns.toolCallsTitle()];
    expect('wording: nothing is called refused', clauses.every(c => !/refused/i.test(c)));
}

function toolCallMalformedChecks(fns, expect) {
    const quiet = f => { try { return f(); } catch (e) { return `THREW ${e}`; } };
    const bad = [null, undefined, 'three', { total: -1 }, { total: 1.5 }, { total: '3' }];
    expect('malformed: bad counts render nothing',
        bad.every(c => quiet(() => fns.toolCallsText(c, 1)) === ''
            && quiet(() => fns.toolCallBreakdown(c)) === ''));
    expect('malformed: references that are not a list give no clause',
        quiet(() => fns.toolCallsText({ total: 4, by_reference: { references: 'kernel', unattached: 0 } }))
            === '4 tool calls');
    expect('malformed: an entry with no name or a bad count is skipped',
        quiet(() => fns.toolCallBreakdown({ total: 4, by_reference: split([{ calls: 2 },
            { name: 'kernel', calls: -1 }, null, { name: 'rdma-core', calls: 2 }]) })) === ' (2 to rdma-core)');
}

// The value of a card's "Tool calls:" line, or null when the card has none.
const toolCallsLine = card => {
    const m = /<strong>Tool calls:<\/strong>([^<]*)<\/div>/.exec(card);
    return m ? m[1].trim() : null;
};

function toolCallCardChecks(fns, expect) {
    const card = r => { try { return String(fns.renderReviewCard(r)); } catch (e) { return `THREW ${e}`; } };
    const reviewed = card({ id: 1, status: 'Reviewed', patch_id: 10, tool_calls: KERNEL_AND_RDMA });
    expect('card: shows the line', toolCallsLine(reviewed) === '350 (72 to kernel, 0 to rdma-core)');
    expect('card: the line explains itself',
        /title="([^"]*)">\s*<strong>Tool calls:/.exec(reviewed)?.[1] === fns.escapeHtml(fns.toolCallsTitle()));
    const seven = { total: 7, by_reference: split([{ name: 'kernel', calls: 2 }]) };
    expect('card: a retry counts its own attempt',
        toolCallsLine(card({ id: 2, status: 'Failed', attempt: 3, tool_calls: seven }))
            === '7 on this attempt (2 to kernel)');
    expect('card: a first attempt carries no marker',
        toolCallsLine(card({ id: 2, status: 'Failed', attempt: 1, tool_calls: seven })) === '7 (2 to kernel)'
        && toolCallsLine(card({ id: 2, status: 'Failed', tool_calls: seven })) === '7 (2 to kernel)');
    expect('card: no call gives no per-reference zero',
        toolCallsLine(card({ id: 3, status: 'Failed', tool_calls:
            { total: 0, by_reference: split([{ name: 'kernel', calls: 0 }]) } })) === '0');
    expect('card: Partial and Failed cards show the line',
        toolCallsLine(card({ id: 4, status: 'Partial', tool_calls: seven })) === '7 (2 to kernel)'
        && toolCallsLine(card({ id: 4, status: 'Failed', tool_calls: seven })) === '7 (2 to kernel)');
    expect('card: In Review and Cancelled cards do not',
        !card({ id: 5, status: 'In Review', tool_calls: seven }).includes('Tool calls')
        && !card({ id: 5, status: 'Cancelled', tool_calls: seven }).includes('Tool calls'));
    expect('card: no counts from the daemon shows a dash',
        toolCallsLine(card({ id: 6, status: 'Reviewed' })) === '—'
        && toolCallsLine(card({ id: 6, status: 'Reviewed', tool_calls: null })) === '—');
    const recorded = card({ id: 7, status: 'Reviewed', patch_id: 10, tool_calls: KERNEL_AND_RDMA,
        stage_durations: [{ stage: 'goal', order: ORDER.goal, seconds: 60, turns: 2 }] });
    const recordedSummary = /data-stage-recorded[\s\S]*?<summary[^>]*>([\s\S]*?)<\/summary>/.exec(recorded);
    expect('card: the recorded stage table carries no tool-call text',
        recordedSummary !== null && !/tool call|to kernel/.test(recordedSummary[1]));
}

// The live stage table: the count of the patch's current run sits in the
// summary beside the stage counts, from the activity poll, and the hover says
// what it sums. Patches 20 and 21 have hosts of their own, so the routing of
// each patch's count to its own table is visible.
const h20 = mkHost(20), h21 = mkHost(21);
const LIVE_STAGES = 'What each review stage of this patch is doing right now.';
const RUN_SENTENCE = "Tool calls: this patch's current run, every attempt.";
const runningStage = pid => ({ key: `patchset:7/patch:${pid}/stage:goal`, patch_id: pid, stage: 'goal',
    order: ORDER.goal, phase: { kind: 'stage', stage: 'goal', turn: 2, max_turns: 50, waiting: { on: 'model' } },
    description: 'stage goal, turn 2/50 (awaiting model)', age_seconds: 30, idle_seconds: 2 });
const RUN_OF_TWO = { patch_id: 20, attempts: 2, total: 47,
    by_reference: split([{ name: 'kernel', calls: 3 }]) };
const summaryOf = host => host.querySelector('summary');

// Paints hosts 20 and 21 through refreshActivity with this response, from a
// blank summary, so a variant that paints nothing cannot pass on what an
// earlier run left there.
async function pollBoth(fns, body) {
    for (const host of [h20, h21]) { summaryOf(host).innerHTML = ''; summaryOf(host).title = ''; }
    respond = () => body;
    try { await fns.refreshActivity(7, fns.state().activityPoll.generation); }
    finally { respond = () => payload; }
    return [h20, h21].map(host => summaryOf(host).textContent);
}

function toolCallPaintChecks(fns, expect) {
    const paint = (live, counts) => {
        fns.paintStageProgress(h20, [runningStage(20)], live, counts);
        return { text: summaryOf(h20).textContent, title: summaryOf(h20).title };
    };
    const counted = paint(true, RUN_OF_TWO);
    expect('live: the summary carries the run\'s count',
        counted.text === 'Stages (1 running) — 47 tool calls over 2 attempts (3 to kernel)');
    expect('live: the title keeps its sentence and says what the count sums',
        counted.title.startsWith(LIVE_STAGES) && counted.title.includes(RUN_SENTENCE)
        && counted.title.includes(fns.toolCallsTitle()));
    const bare = paint(true, null);
    expect('live: no counts paints exactly as before',
        bare.text === 'Stages (1 running)' && bare.title === LIVE_STAGES);
    const persisted = paint(false, RUN_OF_TWO);
    expect('live: a persisted table keeps the count',
        persisted.text.endsWith(' — 47 tool calls over 2 attempts (3 to kernel)')
        && persisted.title.includes(RUN_SENTENCE));
    const before = paint(true, { patch_id: 20, attempts: 1, total: 0,
        by_reference: split([{ name: 'kernel', calls: 0 }]) });
    expect('live: no call yet shows the zero total and no per-reference zero',
        before.text === 'Stages (1 running) — 0 tool calls');
}

async function toolCallRoutingChecks(fns, expect) {
    const entries = [runningStage(20), runningStage(21)];
    // Listed in the opposite order to the entries, so only the patch id can
    // put each count on its host.
    const both = await pollBoth(fns, { live: true, entries, tool_calls: [
        { patch_id: 21, attempts: 1, total: 12, by_reference: null }, RUN_OF_TWO] });
    expect('routing: each patch\'s count lands on its own table',
        both[0] === 'Stages (1 running) — 47 tool calls over 2 attempts (3 to kernel)'
        && both[1] === 'Stages (1 running) — 12 tool calls');
    const older = await pollBoth(fns, { live: true, entries });
    expect('routing: a response without counts paints the label alone',
        older.every(text => text === 'Stages (1 running)'));
    for (const odd of [{}, 'calls', 7]) {
        const shown = await pollBoth(fns, { live: true, entries, tool_calls: odd })
            .catch(e => [`THREW ${e}`]);
        expect(`routing: tool_calls of ${JSON.stringify(odd)} is ignored`,
            shown.length === 2 && shown.every(text => text === 'Stages (1 running)'));
    }
    // A list is not proof that its entries are objects with a patch id; a
    // malformed entry is skipped and the well-formed one still lands.
    const mixed = await pollBoth(fns, { live: true, entries, tool_calls: [
        null, 5, 'x', { patch_id: 'abc', total: 9 }, RUN_OF_TWO] })
        .catch(e => [`THREW ${e}`]);
    expect('routing: malformed entries in tool_calls are skipped',
        mixed.length === 2
        && mixed[0] === 'Stages (1 running) — 47 tool calls over 2 attempts (3 to kernel)'
        && mixed[1] === 'Stages (1 running)');
}

async function toolCallChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    for (const group of [toolCallWordingChecks, toolCallMalformedChecks, toolCallCardChecks,
                         toolCallPaintChecks, toolCallRoutingChecks]) {
        try { await group(fns, expect); } catch (e) { failed.push(`${group.name}: threw ${e}`); }
    }
    return failed;
}
console.log('case 11: tool calls');
{
    const toolCallFailures = await toolCallChecks(ctx);
    check('every tool-call check passes', toolCallFailures.length === 0, toolCallFailures.join(', '));
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
            reason: evil, cause: evil })),
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

// ---- why a stage stopped ----------------------------------------------------
// Each cause has its own words, the same ones the activity descriptions and
// the CLI use, and only a stage's own error is coloured as a fault. A row
// written before the cause existed reads as it always did.
function causeChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    const guard = f => { try { return f(); } catch (e) { return `THREW ${e}`; } };
    const recorded = f => {
        const rows = guard(() => fns.stageRowsFromRecord([], [{ stage: 'goal', order: 0, reason: 'why', ...f }]));
        return Array.isArray(rows) ? String(guard(() => fns.renderStageRow(rows[0]))) : String(rows);
    };
    const live = phase => {
        const rows = guard(() => fns.stageRowsFromActivity([{ patch_id: 1, stage: 'goal', order: 0,
            phase: { kind: 'stage_failed', stage: 'goal', reason: 'why', ...phase } }], true));
        return Array.isArray(rows) ? String(guard(() => fns.renderStageRow(rows[0]))) : String(rows);
    };
    const shows = (html, words) => visibleText(html).includes(`${words}: why`);
    const red = html => html.includes('var(--status-failed)');

    for (const [cause, words] of [
        ['error', 'failed'], ['cancelled', 'cancelled'], ['out_of_time', 'stopped: out of time'],
        ['interrupted', 'still running when the review stopped'],
    ]) {
        const fromRecord = recorded({ cause });
        expect(`cause: a recorded ${cause} reads "${words}"`, shows(fromRecord, words));
        expect(`cause: a live ${cause} reads "${words}"`, shows(live({ cause }), words));
        expect(`cause: a recorded and a live ${cause} render alike`, fromRecord === live({ cause }));
        expect(`cause: only an error is coloured as a fault (${cause})`, red(fromRecord) === (cause === 'error'));
    }
    // A cut-short stage's reason only repeats its label, so the cell reads
    // the label alone and the tooltip says what it means.
    const cut = recorded({ cause: 'cut_short' });
    expect('cause: a recorded cut_short reads "cut short", without its reason',
        visibleText(cut).includes('cut short') && !visibleText(cut).includes(': why'));
    expect('cause: a recorded and a live cut_short render alike', cut === live({ cause: 'cut_short' }));
    expect('cause: a cut-short stage is not coloured as a fault', !red(cut));
    expect('cause: a cause wins over a legacy flag beside it',
        shows(recorded({ cause: 'error', cancelled: true }), 'failed'));
    expect('legacy: cancelled reads "stopped", not "cancelled"',
        shows(recorded({ cancelled: true }), 'stopped') && !red(recorded({ cancelled: true })));
    expect('legacy: not cancelled reads "failed"',
        shows(recorded({ cancelled: false }), 'failed') && red(recorded({ cancelled: false })));
    expect('legacy: no flag at all reads "failed"', shows(recorded({}), 'failed'));
    expect('cause: an unknown cause reads "stopped" and claims no fault',
        shows(recorded({ cause: 'constructor' }), 'stopped') && !red(recorded({ cause: 'constructor' })));
    return failed;
}
console.log('stage stop causes');
{
    const causeFailures = causeChecks(ctx);
    check('every cause check passes', causeFailures.length === 0, causeFailures.join(', '));
}

// ---- a stage that stopped keeps the time it ran ----------------------------
// The daemon records a duration for every stage that ran, and marks one that
// did not finish with `stopped_by`. Such a duration joins its stage's failure
// as one stopped row with the time it ran; it must never read or count as a
// finished stage, and a stage must not appear twice.
function stoppedTimeChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    const guard = f => { try { return f(); } catch (e) { return `THREW ${e}`; } };
    const rowText = (html, label) => String(guard(() => visibleText(rowFor(html, label))));

    const killed = String(guard(() => fns.renderReviewCard({
        id: 20, status: 'Failed', patch_id: 30,
        result: 'Tool error: Review tool timed out (active time exceeded)',
        stage_durations: [
            { stage: 'goal', order: ORDER.goal, seconds: 120, turns: 3 },
            { stage: 'deduplication', order: ORDER.deduplication, seconds: 18, turns: 3,
              stopped_by: 'out_of_time' },
        ],
        stage_failures: [
            { stage: 'deduplication', order: ORDER.deduplication, cause: 'out_of_time',
              reason: 'still running when the review stopped, after 18s and 3 turns' },
        ],
    })));
    const dedup = rowText(killed, 'Stage deduplication');
    expect('stopped time: the killed stage is one row',
        killed.split('>Stage deduplication<').length === 2);
    expect('stopped time: it shows how long it ran and how far it got',
        dedup.includes('18s') && dedup.includes('3 turns'));
    expect('stopped time: it says what stopped it', dedup.includes('stopped: out of time'));
    expect('stopped time: it does not read as done', !/\bdone\b/.test(dedup));
    expect('stopped time: the finished stage still reads as done',
        /\bdone\b/.test(rowText(killed, 'Stage goal')));
    expect('stopped time: the heading counts the time it ran',
        killed.includes('2 stages overlapping')
            && killed.includes(`${fns.formatDuration(138)} summed`));
    const rows = guard(() => fns.stageRowsFromRecord(
        [{ stage: 'deduplication', order: 7, seconds: 18, turns: 3, stopped_by: 'out_of_time' }],
        [{ stage: 'deduplication', order: 7, reason: 'x', cause: 'out_of_time' }]));
    expect('stopped time: no finished row for a stopped stage',
        Array.isArray(rows) && rows.length === 1 && rows[0].state === 'stopped'
            && rows[0].seconds === 18 && rows[0].turns === 3);

    // A stage that wound down and finished is one row, not done and stopped.
    const cut = String(guard(() => fns.renderReviewCard({
        id: 21, status: 'Partial', patch_id: 31,
        stage_durations: [{ stage: 'security', order: ORDER.security, seconds: 900, turns: 31,
                            stopped_by: 'cut_short' }],
        stage_failures: [{ stage: 'security', order: ORDER.security, cause: 'cut_short',
                           reason: 'cut short: the review ran out of time; its findings so far were kept' }],
    })));
    const security = rowText(cut, 'Stage security');
    expect('cut short: one row', cut.split('>Stage security<').length === 2);
    expect('cut short: with its time and label',
        security.includes(fns.formatDuration(900)) && security.includes('31 turns')
            && security.includes('cut short') && !/\bdone\b/.test(security));
    expect('cut short: it reads "cut short after" the time it ran, once',
        security.includes(`cut short after ${fns.formatDuration(900)}`)
            && security.split('cut short').length === 2);
    // The live row of the same stage reads as the recorded one, so a
    // cut-short stage does not change shape when the review ends.
    const recordedCut = guard(() => fns.stageRowsFromRecord(
        [{ stage: 'security', order: ORDER.security, seconds: 900, turns: 31, stopped_by: 'cut_short' }],
        [{ stage: 'security', order: ORDER.security, cause: 'cut_short',
           reason: 'cut short: the review ran out of time; its findings so far were kept' }]));
    const liveCut = guard(() => fns.stageRowsFromActivity([{
        patch_id: 31, stage: 'security', order: ORDER.security,
        phase: { kind: 'stage_done', stage: 'security', seconds: 900, turns: 31, cut_short: true },
        description: 'stage security cut short after 15m 0s, 31 turns', age_seconds: 4 }], true));
    expect('cut short: the live finish is a stopped row, not a done one',
        Array.isArray(liveCut) && liveCut[0].state === 'stopped' && liveCut[0].cause === 'cut_short');
    expect('cut short: live and recorded render alike',
        Array.isArray(liveCut) && Array.isArray(recordedCut)
            && String(guard(() => fns.renderStageRow(liveCut[0])))
                === String(guard(() => fns.renderStageRow(recordedCut[0]))));
    expect('cut short: a live table counts it as stopped',
        Array.isArray(liveCut) && fns.stageTableLabel(liveCut, true) === 'Stages (0 running, 1 stopped)');
    const plainDone = guard(() => fns.stageRowsFromActivity([{
        patch_id: 31, stage: 'goal', order: 0,
        phase: { kind: 'stage_done', stage: 'goal', seconds: 9, turns: 1, cut_short: false } }], true));
    expect('cut short: a finish that was not cut short is still done',
        Array.isArray(plainDone) && plainDone[0].state === 'done');

    // A stopped duration with no failure beside it still reads as stopped.
    const alone = guard(() => fns.stageRowsFromRecord(
        [{ stage: 'locking', order: 4, seconds: 18, turns: 1, stopped_by: 'interrupted' }], []));
    const aloneHtml = Array.isArray(alone) ? String(guard(() => fns.renderStageRow(alone[0]))) : String(alone);
    expect('alone: a stopped duration with no failure is a stopped row',
        Array.isArray(alone) && alone.length === 1 && alone[0].state === 'stopped');
    expect('alone: it reads by its own cause, with its time and no invented reason',
        visibleText(aloneHtml).includes('still running when the review stopped')
            && visibleText(aloneHtml).includes('18s') && visibleText(aloneHtml).includes('1 turn')
            && !aloneHtml.includes('no reason given'));
    return failed;
}
console.log('stopped stages keep their time');
{
    const stoppedFailures = stoppedTimeChecks(ctx);
    check('every stopped-time check passes', stoppedFailures.length === 0, stoppedFailures.join(', '));
}

// ---- partial and timed-out reviews -----------------------------------------
// A review that ran out of time keeps the findings of the stages that
// finished and is Partial: its findings are shown and counted like a Reviewed
// review's, but nothing claims it ran to the end or showed the code clean. A
// patch whose review ran out of time with nothing salvaged is TimedOut.
function partialChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    const guard = f => { try { return String(f()); } catch (e) { return `THREW ${e}`; } };
    const output = findings => JSON.stringify({ summary: 'what was found', findings });
    const CRITICAL = { severity: 'Critical', problem: 'use after free' };

    // The patchset list.
    const listed = (status, counts) => {
        try { return fns.renderPatchsetListStatus({ status, ...counts }); }
        catch (e) { return { statusHtml: `THREW ${e}`, findingsHtml: `THREW ${e}` }; }
    };
    const partialRow = listed('Partial', { findings_low: 0, findings_medium: 0, findings_high: 0,
                                           findings_critical: 1 });
    expect('list: a Partial patchset wears its own badge',
        partialRow.statusHtml.includes('status-Partial') && partialRow.statusHtml.includes('>Partial<'));
    expect('list: its critical finding is counted',
        /color:var\(--sev-critical\)[^>]*title="Critical">1</.test(partialRow.findingsHtml));
    const zero = { findings_low: 0, findings_medium: 0, findings_high: 0, findings_critical: 0 };
    expect('list: a Partial patchset with no findings shows "-", not the check mark',
        listed('Partial', zero).findingsHtml.includes('-') && !listed('Partial', zero).findingsHtml.includes('✓'));
    expect('list: a Reviewed patchset with no findings keeps the check mark',
        listed('Reviewed', zero).findingsHtml.includes('✓'));
    expect('list: the status is escaped',
        !listed('<b>x</b>', zero).statusHtml.includes('<b>'));

    // The per-patch summary table.
    const summary = (patch, reviews) => guard(() => fns.patchSummaryStatus(patch, reviews));
    const partialReview = { status: 'Partial', output: output([CRITICAL]) };
    const salvaged = summary({ status: 'Partial' }, [partialReview]);
    expect('summary: a Partial review shows its findings',
        /title="Critical">1</.test(salvaged) && !salvaged.includes('In progress'));
    expect('summary: tagged partial', visibleText(salvaged).includes('partial'));
    const rerun = summary({ status: 'Reviewed' },
        [partialReview, { status: 'Reviewed', output: output([]) }]);
    expect('summary: a Reviewed review takes over from a Partial one',
        rerun.includes('✓') && !visibleText(rerun).includes('partial'));
    expect('summary: the runs note leaves out the Partial run the badges leave out',
        !visibleText(rerun).includes('max of'));
    const twoReviewed = summary({ status: 'Reviewed' }, [partialReview,
        { status: 'Reviewed', output: output([]) }, { status: 'Reviewed', output: output([]) }]);
    expect('summary: two Reviewed runs beside a Partial one are "max of 2 runs"',
        visibleText(twoReviewed).includes('(max of 2 runs)'));
    const twoPartial = summary({ status: 'Partial' },
        [partialReview, { status: 'Partial', output: output([]) }]);
    expect('summary: two Partial runs carry the runs note',
        visibleText(twoPartial).includes('(max of 2 runs)') && visibleText(twoPartial).includes('partial'));
    expect('summary: a single Reviewed run after a failed one keeps the old runs note',
        visibleText(summary({ status: 'Reviewed' },
            [{ status: 'Failed' }, { status: 'Reviewed', output: output([]) }])).includes('(max of 2 runs)'));
    const afterFailure = summary({ status: 'Partial' }, [{ status: 'Failed' }, partialReview]);
    expect('summary: a Partial review wins over a failed one',
        visibleText(afterFailure).includes('partial') && !afterFailure.includes('Failed'));
    expect('summary: a Partial review with no findings shows "-"',
        !summary({ status: 'Partial' }, [{ status: 'Partial', output: output([]) }]).includes('✓'));
    const timedOut = summary({ status: 'TimedOut' }, [{ status: 'Failed' }]);
    expect('summary: a TimedOut patch says it timed out',
        timedOut.includes('status-TimedOut') && visibleText(timedOut).includes('Timed out'));
    expect('summary: a Failed patch still shows the failure',
        visibleText(summary({ status: 'Failed' }, [{ status: 'Failed' }])).includes('Failed'));

    // The review card.
    const card = guard(() => fns.renderReviewCard({
        id: 50, status: 'Partial', patch_id: 60, attempt: 2, duration_seconds: 9000,
        output: output([CRITICAL]), summary: 'the summary text', inline_review: 'the inline review',
        stage_durations: [{ stage: 'security', order: ORDER.security, seconds: 900, turns: 31,
                            stopped_by: 'cut_short' }],
        stage_failures: [{ stage: 'security', order: ORDER.security, cause: 'cut_short',
                           reason: 'cut short: the review ran out of time; its findings so far were kept' }],
    }));
    expect('card: its findings are shown', card.includes('Critical: <span') && card.includes('>1<'));
    expect('card: its summary is shown', card.includes('the summary text'));
    expect('card: its inline review is shown', card.includes('the inline review'));
    expect('card: no live-progress host hides them', !card.includes('data-stage-progress'));
    expect('card: says it stopped out of time',
        card.includes(`Stopped (out of time) after ${fns.formatDuration(9000)}`));
    expect('card: never claims to have succeeded', !card.includes('succeeded'));
    expect('card: the attempt is still reported', card.includes('on attempt 2'));
    expect('card: says where its findings come from',
        card.includes('Any findings below come only from the stages that finished.'));
    expect('card: the note sits under the coverage banner',
        card.indexOf('Incomplete coverage') >= 0
            && card.indexOf('Incomplete coverage') < card.indexOf('Any findings below'));
    expect('card: the badge is Partial', card.includes('status-badge status-Partial'));
    const nothingFound = guard(() => fns.renderReviewCard({
        id: 51, status: 'Partial', patch_id: 61, output: output([]), inline_review: 'nothing' }));
    expect('card: a Partial review with no findings does not claim no regressions',
        !nothingFound.includes('No regressions')
            && nothingFound.includes('No findings from the stages that finished'));
    expect('card: and does not open its inline review as if it found something',
        nothingFound.includes('▶ Inline Review') && !nothingFound.includes('▼ Inline Review'));
    expect('card: a Partial review that found something opens its findings',
        card.includes('▼ Findings (1)') && card.includes('▶ Inline Review'));
    const reviewed = guard(() => fns.renderReviewCard({
        id: 52, status: 'Reviewed', patch_id: 62, output: output([]) }));
    expect('card: a Reviewed review with no findings still reads no regressions',
        reviewed.includes('No regressions') && !reviewed.includes('Any findings below'));

    // The statistics.
    const cards = guard(() => fns.renderReviewStatCards({
        total_reviews: 10, total_failures: 2, total_partial: 3,
        reviews: [{ status: 'Reviewed', count: 5 }, { status: 'Partial', count: 3 },
                  { status: 'Failed', count: 2 }, { status: 'In Review', count: 4 }],
    }));
    expect('stats: a Partial card with its count',
        /Total Partial<\/h3>\s*<div class="value">3</.test(cards));
    expect('stats: failures do not include it', /Total Failures<\/h3>\s*<div class="value">2</.test(cards));
    expect('stats: the success rate is Reviewed over every finished review',
        /Success Rate[^<]*<\/h3>\s*<div class="value">50%</.test(cards));
    expect('stats: an older server without the count reads 0',
        /Total Partial<\/h3>\s*<div class="value">0</.test(
            guard(() => fns.renderReviewStatCards({ total_reviews: 1, total_failures: 0, reviews: [] }))));
    const colours = { secondary: 'green', primary: 'blue', failure: 'red' };
    expect('stats: the chart colours Partial with the theme colour',
        guard(() => fns.reviewOutcomeColor('Partial', colours)) === '#9a6700');
    expect('stats: and still colours a failure as one',
        guard(() => fns.reviewOutcomeColor('Failed To Apply', colours)) === 'red');
    return failed;
}
console.log('partial and timed-out reviews');
{
    const partialFailures = partialChecks(ctx);
    check('every partial check passes', partialFailures.length === 0, partialFailures.join(', '));

    // Each theme gives both statuses a colour, and every badge and dot form
    // has a class, since badges are built as status-<status without spaces>.
    const themes = (src.match(/--status-failed:/g) || []).length;
    for (const v of ['--status-partial:', '--status-timedout:']) {
        check(`every theme defines ${v}`, (src.match(new RegExp(v, 'g')) || []).length === themes,
            `${themes} themes`);
    }
    check('there are eight themes', themes === 8, String(themes));
    for (const cls of ['.status-Partial {', '.status-TimedOut {',
                       '.status-dot.status-Partial {', '.status-dot.status-TimedOut {']) {
        check(`the page styles ${cls.slice(0, -2)}`, src.includes(cls));
    }
}

// ---- the findings on a review card -----------------------------------------
// A finished card lists the findings its Result line counts, worst first, each
// with its title and tags and opening onto why it got its level and where it
// is. The list starts open and the inline review then starts collapsed, so a
// card never opens on the same findings twice. Model text is escaped
// everywhere, and an entry the counts cannot read gives no list at all.
const findingsCard = (fns, findings, extra = {}) => {
    try {
        return String(fns.renderReviewCard({ id: 70, status: 'Reviewed', patch_id: 80,
            output: JSON.stringify({ findings }), inline_review: 'the inline review', ...extra }));
    } catch (e) { return `THREW ${e}`; }
};
// The Findings block of a card: from its button to the inline review's.
const findingsBlock = card => {
    const start = card.indexOf('Findings (');
    if (start < 0) return '';
    const end = card.indexOf('Inline Review', start);
    return card.slice(start, end < 0 ? card.length : end);
};

function findingTitleChecks(fns, expect) {
    const guard = f => { try { return f(); } catch (e) { return { title: `THREW ${e}`, clipped: null }; } };
    const short = 'net: ionic: leak of the rx ring in ionic_lif_open() when the queue fails';
    const shortTitle = guard(() => fns.findingTitle({ problem: `  ${short}  ` }));
    expect('title: a short problem is the title, trimmed',
        shortTitle.title === short && shortTitle.clipped === false);
    expect('title: an empty or missing problem reads "(no description)"',
        ['   ', undefined, 7].every(problem =>
            guard(() => fns.findingTitle({ problem })).title === '(no description)'));
    const shortCard = findingsCard(fns, [{ severity: 'High', problem: short, severity_explanation: 'why' }]);
    expect('title: a short title is not repeated in the body',
        visibleText(shortCard).split(short).length === 2);

    // A multibyte character at the cut: the clip keeps it whole.
    const long = 'a'.repeat(159) + '😀' + ' b'.repeat(120);
    const clipped = guard(() => fns.findingTitle({ problem: long }));
    expect('title: a long problem is clipped at the limit and ends in "…"',
        clipped.clipped === true && Array.from(clipped.title).length === 161
            && clipped.title.endsWith('😀…'));
    const exact = guard(() => fns.findingTitle({ problem: 'y'.repeat(160) }));
    expect('title: a problem exactly at the limit is not clipped',
        exact.title === 'y'.repeat(160) && exact.clipped === false);
    expect('title: trailing space at the cut is trimmed',
        guard(() => fns.findingTitle({ problem: 'a'.repeat(159) + ' ' + 'b'.repeat(50) })).title
            === 'a'.repeat(159) + '…');
    const longCard = findingsCard(fns, [{ severity: 'Low', problem: long }]);
    expect('title: a clipped title puts the full problem in the body',
        longCard.includes(long) && longCard.includes('<details class="finding">'));
}

function findingOrderChecks(fns, expect) {
    const ordered = findingsCard(fns, [
        { severity: 'Medium', problem: 'm-one' },
        { severity: 'High', problem: 'h-one' },
        { severity: 'high', problem: 'h-two' },
    ]);
    const at = name => ordered.indexOf(`>${name}<`);
    expect('order: "Findings (3)"', ordered.includes('▼ Findings (3)'));
    expect('order: severest first, equal severities in the model\'s order',
        at('h-one') > 0 && at('h-one') < at('h-two') && at('h-two') < at('m-one'));
    const unknown = findingsCard(fns, [
        { severity: 'bogus', problem: 'u-one' }, { severity: 'Low', problem: 'l-one' },
        { severity: 'Medium', problem: 'm-two' },
    ]);
    expect('order: an unknown severity is listed with the Lows',
        unknown.indexOf('>m-two<') < unknown.indexOf('>u-one<')
            && unknown.indexOf('>u-one<') < unknown.indexOf('>l-one<'));
    expect('order: a severity is coloured by its level',
        ordered.includes('color:var(--sev-high);">High<')
            && ordered.includes('color:var(--sev-medium);">Medium<'));
}

function findingCardChecks(fns, expect) {
    const card = findingsCard(fns, [{ severity: 'High', problem: 'h', severity_explanation: 'why' }],
        { summary: 'the summary' });
    expect('card: the findings block starts open',
        card.includes('▼ Findings (1)') && /id="findings-[^"]+" class="collapsible open"/.test(card));
    expect('card: the inline review starts collapsed beside it',
        card.includes('▶ Inline Review') && !card.includes('▼ Inline Review'));
    expect('card: summary, then findings, then inline review',
        card.indexOf('the summary') < card.indexOf('Findings (')
            && card.indexOf('Findings (') < card.indexOf('Inline Review'));

    const both = [{ severity: 'High', problem: 'new-one' },
                  { severity: 'High', problem: 'old-one', preexisting: true }];
    const patchset = findingsCard(fns, both, { patch_id: null });
    expect('card: a patchset-level card leaves preexisting findings out, as its counts do',
        patchset.includes('Findings (1)') && !patchset.includes('old-one')
            && /High: <span[^>]*>1</.test(patchset));
    const patch = findingsCard(fns, both);
    expect('card: a patch card lists them, tagged',
        patch.includes('Findings (2)') && findingsBlock(patch).includes('(preexisting)')
            && findingsBlock(patch).split('(preexisting)').length === 2);
    expect('tags: only a JSON true is preexisting',
        fns.findingTags({ preexisting: 'yes' }).length === 0
            && fns.findingTags({ preexisting: true }).join('').includes('(preexisting)'));

    const reasoned = findingsBlock(findingsCard(fns, [{ severity: 'High', problem: 'h',
        severity_explanation: 'it crashes on every boot' }]));
    const reasonedBody = reasoned.slice(reasoned.indexOf('<div class="finding-body">'));
    expect('card: a finding opens onto its severity reasoning',
        reasoned.includes('<details class="finding">') && reasonedBody.includes('it crashes on every boot'));

    const bare = findingsCard(fns, [{ severity: 'Low', problem: 'bare' }]);
    expect('card: a finding with no body is a plain row',
        bare.includes('<div class="finding">') && !bare.includes('<details class="finding"'));

    expect('card: no findings, no block', !findingsCard(fns, []).includes('Findings ('));
    for (const [what, output] of [['unparseable output', '{ not json'],
                                  ['output with no findings array', JSON.stringify({ summary: 's' })]]) {
        const c = findingsCard(fns, [], { output });
        expect(`card: ${what} gives no block and keeps the inline review collapsed`,
            !c.includes('Findings (') && c.includes('▶ Inline Review') && !c.includes('THREW'));
    }
}

function findingMalformedChecks(fns, expect) {
    const withNull = findingsCard(fns, [null, { severity: 'High', problem: 'x' }]);
    expect('malformed: a null entry renders the card without a list',
        !withNull.includes('THREW') && !withNull.includes('Findings (')
            && withNull.includes('font-weight:bold;">Finished<'));
    const withString = findingsCard(fns, ['junk', { severity: 'High', problem: 'x' }]);
    const block = findingsBlock(withString);
    expect('malformed: a string entry is listed as a Low "(no description)"',
        withString.includes('Findings (2)')
            && /color:var\(--sev-low\);">Low<\/span><span[^>]*>\(no description\)</.test(block));
    expect('malformed: and the Result line counts the same two',
        /High: <span[^>]*>1</.test(withString) && /Low: <span[^>]*>1</.test(withString));
}

function findingEscapingChecks(fns, expect) {
    const evil = '<script>x</script>"onerror=alert(1)\'';
    const card = findingsCard(fns, [{ severity: 'High', problem: evil, severity_explanation: evil,
        locations: [{ file: evil, line: 3, function_or_symbol: evil, why_this_location_matters: evil }] }]);
    const block = findingsBlock(card);
    expect('escaping: no raw markup or attribute break-out in a finding',
        block.length > 0 && !block.includes('<script>') && !block.includes('"onerror'));
    // Six places show the text: the title and its tooltip, the severity
    // reasoning, the file and function in the location, and why it matters.
    // An exact count catches any one of them going missing.
    expect('escaping: problem, reasoning and location shown escaped',
        block.split('&lt;script&gt;').length === 7);
    const injected = findingsBlock(findingsCard(fns, [{ severity: 'low;" onmouseover="x', problem: 'p' }]));
    expect('escaping: a severity is neither injected nor coloured',
        !injected.includes('" onmouseover="x')
            && injected.includes('color:var(--sev-unknown);">low;&quot; onmouseover=&quot;x<'));
}

function findingLocationChecks(fns, expect) {
    const where = locations => visibleText(String(fns.renderFindingLocations(locations))).trim();
    expect('location: file:line (function)',
        where([{ file: 'f.c', line: 12, function_or_symbol: 'fn' }]) === 'f.c:12 (fn)');
    expect('location: absent parts are dropped',
        where([{ file: 'f.c', function_or_symbol: 'fn' }]) === 'f.c (fn)'
            && where([{ file: 'f.c', line: 12 }]) === 'f.c:12'
            && where([{ function_or_symbol: 'fn' }]) === 'fn');
    expect('location: only a non-negative integer is a line',
        where([{ file: 'f.c', line: '12' }]) === 'f.c' && where([{ file: 'f.c', line: -1 }]) === 'f.c'
            && where([{ file: 'f.c', line: 12.5 }]) === 'f.c');
    expect('location: non-objects and non-arrays render nothing',
        fns.renderFindingLocations(['f.c', null, 4]) === '' && fns.renderFindingLocations('f.c') === '');
    const card = findingsCard(fns, [{ severity: 'Medium', problem: 'p', locations: [
        { file: 'f.c', line: 12, function_or_symbol: 'fn', why_this_location_matters: 'the ring leaks here',
          code_snippet: 'SNIPPET' }] }]);
    expect('location: the card shows it with why it matters, without the snippet',
        card.includes('<code>f.c:12 (fn)</code>') && card.includes('the ring leaks here')
            && !card.includes('SNIPPET'));
}

// What `parseSeverityCalibration` makes of an explanation, as [label, body]
// pairs, so a check can state the whole split at once.
const calibration = (fns, text) => {
    const r = fns.parseSeverityCalibration(text);
    return r && { preamble: r.preamble, sections: r.sections.map(x => [x.label, x.body]) };
};
const sameSplit = (got, preamble, sections) => !!got && got.preamble === preamble
    && JSON.stringify(got.sections) === JSON.stringify(sections);

function findingCalibrationLabelChecks(fns, expect) {
    const split = text => calibration(fns, text);
    expect('calibration: all three kernel labels, in order, each body stopping at the next',
        sameSplit(split('Consequence: a leak. Triggering path: a caller passes Y. Reachability: ioctl.'), '',
            [['Consequence', 'a leak'], ['Triggering path', 'a caller passes Y'], ['Reachability', 'ioctl.']]));
    expect('calibration: the Sashiko headings, in the plain form',
        sameSplit(split('Blast radius and irreversibility: one queue. Likelihood and preconditions: rare.'), '',
            [['Blast radius and irreversibility', 'one queue'], ['Likelihood and preconditions', 'rare.']]));
    expect('calibration: the short Sashiko headings',
        sameSplit(split('Blast radius: one queue.\nLikelihood: rare.'), '',
            [['Blast radius', 'one queue.'], ['Likelihood', 'rare.']]));
    const bullets = split('- **Blast radius and irreversibility**: data loss.\n- **Likelihood and preconditions**: needs root.');
    expect('calibration: the bold bullet form, with no markup left in a body',
        sameSplit(bullets, '', [['Blast radius and irreversibility', 'data loss.'],
                                ['Likelihood and preconditions', 'needs root.']]));
    expect('calibration: the colon may sit inside the bold',
        sameSplit(split('**Consequence:** a. **Triggering path:** b'), '',
            [['Consequence', 'a'], ['Triggering path', 'b']])
            && sameSplit(split('**Consequence:** a.\n**Triggering path:** b'), '',
                [['Consequence', 'a.'], ['Triggering path', 'b']]));
    expect('calibration: labels on their own lines give two sections',
        sameSplit(split('Consequence: a.\nTriggering path: b.'), '',
            [['Consequence', 'a.'], ['Triggering path', 'b.']]));
    expect('calibration: a label after a blank line, and after two spaces',
        sameSplit(split('Consequence: a.\n\nReachability: c'), '',
            [['Consequence', 'a.'], ['Reachability', 'c']])
            && sameSplit(split('Consequence: a.  Triggering path: b'), '',
                [['Consequence', 'a'], ['Triggering path', 'b']]));
    expect('calibration: text before the first label is the preamble',
        sameSplit(split('It breaks. Consequence: a leak.'), 'It breaks',
            [['Consequence', 'a leak.']]));
    expect('calibration: only the first occurrence of a label opens a section',
        sameSplit(split('Consequence: a.\nConsequence: b.'), '', [['Consequence', 'a.\nConsequence: b.']]));
    expect('calibration: a section with an empty body is dropped',
        sameSplit(split('Consequence:\nTriggering path: b'), '', [['Triggering path', 'b']]));
}

function findingCalibrationShapeChecks(fns, expect) {
    const split = text => calibration(fns, text);
    // Shapes the deployments' explanations take (kernel-fork #35,
    // driver-a #59, #42, #153).
    const list = split('1. Consequence: a.\n2. Triggering path: b.\n3. Reachability: c.');
    expect('calibration: a numbered list gives three sections and no stray list number',
        sameSplit(list, '', [['Consequence', 'a.'], ['Triggering path', 'b.'], ['Reachability', 'c.']]));
    expect('calibration: the same list on one line gives two sections',
        sameSplit(split('x. 1. Consequence: a. 2. Triggering path: b.'), 'x',
            [['Consequence', 'a'], ['Triggering path', 'b.']]));
    expect('calibration: a parenthesised qualifier stays in the label',
        sameSplit(split('Consequence: a.\nTriggering path (verified, not speculative): b'), '',
            [['Consequence', 'a.'], ['Triggering path (verified, not speculative)', 'b']]));
    expect('calibration: a plain qualifier stays in the label',
        sameSplit(split('Consequence: a.\nTriggering path verified in code: b'), '',
            [['Consequence', 'a.'], ['Triggering path verified in code', 'b']]));
    expect('calibration: "Consequence first:" opens a section',
        sameSplit(split('Consequence first: a'), '', [['Consequence first', 'a']]));
    expect('calibration: a number that ends a sentence stays in the previous body',
        sameSplit(split('Consequence: it leaves it at 1. Triggering path: b'), '',
            [['Consequence', 'it leaves it at 1'], ['Triggering path', 'b']]));
    expect('calibration: a closing parenthesis stays in the body before a label on the same line',
        sameSplit(split('Consequence: crash (null deref) Triggering path: x'), '',
            [['Consequence', 'crash (null deref)'], ['Triggering path', 'x']])
            && sameSplit(split('It breaks (badly) Consequence: a'), 'It breaks (badly)', [['Consequence', 'a']]));
    expect('calibration: a bullet marker before a label is not carried into either body',
        sameSplit(split('* Consequence: a.\n* Reachability: c.'), '',
            [['Consequence', 'a.'], ['Reachability', 'c.']]));
}

function findingCalibrationProseChecks(fns, expect) {
    const split = text => calibration(fns, text);
    expect('calibration: prose that names a label is not a section',
        sameSplit(split('Consequence: nothing, because reachability is speculative here.'), '',
            [['Consequence', 'nothing, because reachability is speculative here.']]));
    expect('calibration: "Not lowered for reachability:" is prose',
        sameSplit(split('Not lowered for reachability: x'), 'Not lowered for reachability: x', []));
    expect('calibration: a lowercase label after a sentence end is prose',
        sameSplit(split('Consequence: a. consequence: x'), '', [['Consequence', 'a. consequence: x']]));
    expect('calibration: a label after unpunctuated text on the line is prose',
        sameSplit(split('Consequence: a, the Reachability: x'), '',
            [['Consequence', 'a, the Reachability: x']]));
    expect('calibration: a half-bold label is prose',
        sameSplit(split('**Consequence: x'), '**Consequence: x', [])
            && sameSplit(split('Consequence**: x'), 'Consequence**: x', []));
    expect('calibration: a label with no colon is prose',
        sameSplit(split('Consequence is bad. Reachability is fine.'), 'Consequence is bad. Reachability is fine.', []));
    expect('calibration: an unlabelled numbered list is all preamble',
        sameSplit(split('1. Condition Y is met.\n2. The buffer leaks.'), '1. Condition Y is met.\n2. The buffer leaks.', []));
    expect('calibration: empty and whitespace text give null',
        ['', '   \n\t', undefined, null, 7].every(text => fns.parseSeverityCalibration(text) === null));
    const text = 'Consequence: a. Triggering path: b. Reachability: c.';
    const first = JSON.stringify(fns.parseSeverityCalibration(text));
    expect('calibration: repeated calls agree, so no regex state carries over',
        [1, 2, 3].every(() => JSON.stringify(fns.parseSeverityCalibration(text)) === first)
            && JSON.parse(first).sections.length === 3);
}

function findingCalibrationRenderChecks(fns, expect) {
    const rendered = text => String(fns.renderSeverityCalibration(text));
    expect('calibration: nothing to render for empty text',
        rendered('') === '' && rendered('  ') === '' && rendered(undefined) === '');
    const html = rendered('It breaks. Consequence: a leak. Reachability: ioctl.');
    expect('calibration: the preamble, then one line per step with its label in muted bold',
        html.indexOf('It breaks') >= 0 && html.indexOf('It breaks') < html.indexOf('Consequence:')
            && html.indexOf('Consequence:') < html.indexOf('Reachability:')
            && (html.match(/<span class="finding-label">/g) || []).length === 2
            && visibleText(html).replace(/\s+/g, ' ').includes('Consequence: a leak'));
    const unlabelled = rendered('just prose, no labels');
    expect('calibration: an unlabelled explanation is shown whole',
        unlabelled.includes('just prose, no labels') && !unlabelled.includes('finding-label'));
    const evil = '<img src=x onerror=alert(1)>';
    const escaped = rendered(`${evil}. Consequence (${evil.slice(1, 4)}): ${evil}`);
    expect('calibration: model text is escaped in the preamble, the label and the body',
        !escaped.includes('<img') && escaped.split('&lt;img').length === 3);
    const card = findingsCard(fns, [{ severity: 'High', problem: 'h',
        severity_explanation: 'Consequence: it crashes. Triggering path: boot. Reachability: always.' }]);
    expect('calibration: a card opens onto the split reasoning',
        card.includes('<span class="finding-label">Triggering path:</span> boot')
            && card.includes('<span class="finding-label">Reachability:</span> always.'));
}

// The speculative tag and the note for findings that do not say either way.
function findingSpeculativeChecks(fns, expect) {
    const tagText = f => fns.findingTags(f).join('');
    expect('speculative: true tags the finding',
        tagText({ speculative: true }).includes('(speculative)'));
    expect('speculative: false, absent and "true" do not',
        [{ speculative: false }, {}, { speculative: 'true' }, { speculative: 1 }, null].every(f =>
            !tagText(f).includes('(speculative)')));
    expect('speculative: the tooltip says the severity is capped',
        tagText({ speculative: true }).includes(
            'title="The reviewer could not establish how this bug is triggered, so its severity is capped"'));
    expect('speculative: it is read from the key, not from the reasoning',
        !tagText({ speculative: false,
                   severity_explanation: 'Triggering path (verified, not speculative): boot' })
            .includes('(speculative)'));
    expect('speculative: the tag comes before the preexisting tag',
        (() => {
            const text = tagText({ speculative: true, preexisting: true });
            return text.includes('(speculative)') && text.indexOf('(speculative)') < text.indexOf('(preexisting)');
        })());

    const high = findingsBlock(findingsCard(fns,
        [{ severity: 'High', problem: 'h-one', speculative: true }]));
    expect('speculative: a High finding shows the tag too',
        high.includes('>High<') && high.includes('(speculative)'));
    const mixed = findingsBlock(findingsCard(fns, [
        { severity: 'High', problem: 'h-one', speculative: true },
        { severity: 'Low', problem: 'l-one', speculative: false }]));
    expect('speculative: only the marked row carries the tag',
        mixed.split('(speculative)').length === 2
            && mixed.indexOf('(speculative)') < mixed.indexOf('>l-one<'));

    const NOTE = 'These findings do not say whether they are speculative.';
    const countNotes = findings => findingsBlock(findingsCard(fns, findings)).split(NOTE).length - 1;
    const unmarked = [{ severity: 'High', problem: 'a' }, { severity: 'Low', problem: 'b' },
                      { severity: 'Low', problem: 'c' }];
    expect('note: shown once when no finding says whether it is speculative', countNotes(unmarked) === 1);
    const unmarkedBlock = findingsBlock(findingsCard(fns, unmarked));
    expect('note: it opens the list, before the first row',
        unmarkedBlock.indexOf(NOTE) >= 0 && unmarkedBlock.indexOf(NOTE) < unmarkedBlock.indexOf('>a<'));
    expect('note: not shown when one finding says false',
        countNotes([...unmarked, { severity: 'Low', problem: 'd', speculative: false }]) === 0);
    expect('note: not shown when one finding says true',
        countNotes([...unmarked, { severity: 'Low', problem: 'd', speculative: true }]) === 0);
    expect('note: a value that is not a boolean does not count as marked',
        countNotes([{ severity: 'High', problem: 'a', speculative: 'true' },
                    { severity: 'Low', problem: 'b', speculative: null }]) === 1);
    const patchset = findingsCard(fns, [{ severity: 'High', problem: 'new-one' },
        { severity: 'High', problem: 'old-one', preexisting: true, speculative: false }], { patch_id: null });
    expect('note: a finding the card does not list cannot mark the others',
        findingsBlock(patchset).includes(NOTE) && !patchset.includes('old-one'));
    expect('note: absent from a card with no findings block',
        !findingsCard(fns, []).includes(NOTE));
}

// The Findings button must open the block it sits beside. The id is built in
// two places of one template, so a typo in either leaves a button that does
// nothing and no error anywhere.
function findingToggleChecks(fns, expect) {
    const card = findingsCard(fns, [{ severity: 'High', problem: 'h' }]);
    const toggled = /toggleCollapsible\('([^']*)'/.exec(card);
    const opened = /id="(findings-[^"]*)" class="collapsible open"/.exec(card);
    expect('toggle: the Findings button opens exactly the block it sits beside',
        !!toggled && !!opened && toggled[1] === opened[1]);
    expect('toggle: the id is built from the review, never "undefined"',
        !!toggled && toggled[1].startsWith('findings-') && !card.includes('undefined')
            && toggled[1] !== 'findings-');
}

// Padded and blank location parts are trimmed or dropped, as the CLIs do, so a
// location never prints as "a.c  (   )".
function findingLocationTrimChecks(fns, expect) {
    const where = locations => visibleText(String(fns.renderFindingLocations(locations))).trim();
    expect('location trim: padded parts are trimmed and a blank function is dropped',
        where([{ file: '  a.c  ', function_or_symbol: '  ' }]) === 'a.c');
    expect('location trim: a padded line and function keep one space between them',
        where([{ file: '  a.c ', line: 3, function_or_symbol: ' fn ' }]) === 'a.c:3 (fn)');
    expect('location trim: a blank file leaves the function alone',
        where([{ file: ' \t ', function_or_symbol: ' fn ' }]) === 'fn');
    expect('location trim: a location with only blank parts is dropped',
        fns.renderFindingLocations([{ file: ' ', function_or_symbol: ' ',
            why_this_location_matters: ' ' }]) === '');
}

// A line number is a non-negative integer a JSON reader can hold exactly, the
// CLIs' rule: 1e22 is an integer to JavaScript but prints as "1e+22".
function findingLineNumberChecks(fns, expect) {
    const shown = line => visibleText(String(fns.renderFindingLocations([{ file: 'f.c', line }]))).trim();
    for (const line of [12, 0, 9007199254740991]) {
        expect(`line number: ${line} is shown`, shown(line) === `f.c:${line}`);
    }
    for (const line of [1e22, 9007199254740992, 12.5, -1, '12', null]) {
        expect(`line number: ${String(line)} is not shown`, shown(line) === 'f.c');
    }
}

// A whitespace-only severity is Low on the page, as in the severity counts. A
// padded severity is not trimmed, because the counts and the ordering do not
// trim it either: the row must not colour it as a level they do not count.
function findingBlankSeverityChecks(fns, expect) {
    const block = findingsBlock(findingsCard(fns, [{ severity: '  ', problem: 'p' }]));
    expect('severity: a whitespace-only severity reads Low',
        block.includes('color:var(--sev-low);">Low<'));
    const padded = findingsBlock(findingsCard(fns, [{ severity: ' High ', problem: 'p' }]));
    expect('severity: a padded severity is not coloured as the level it pads',
        !padded.includes('--sev-high'));
    const counts = fns.countNewFindings({ output: { findings: [{ severity: ' High ', problem: 'p' }] } });
    expect('severity: the row and the Result line agree on a padded severity',
        counts.high === 0 && counts.low === 1);
}

// The order of a row inside its <details>: the summary reads severity, tags,
// title; the body reads the full problem (when the title was clipped), the
// severity reasoning, then the locations. Every block is checked against the
// next, so swapping any two is caught.
function findingRowLayoutChecks(fns, expect) {
    const problem = 'FULLPROBLEM ' + 'x'.repeat(200);
    const block = findingsBlock(findingsCard(fns, [{
        severity: 'High', problem, speculative: true, preexisting: true,
        severity_explanation: 'Triggering path: REASONMARK', locations: [{ file: 'loc.c', line: 4 }],
    }]));
    const details = /<details class="finding"><summary>([\s\S]*?)<\/summary><div class="finding-body">([\s\S]*)<\/div><\/details>/
        .exec(block);
    expect('layout: a finding with a body is one details with a summary and a body', !!details);
    if (!details) return;
    const [, summary, body] = details;
    const inOrder = (text, marks) => marks.every(mark => text.indexOf(mark) >= 0)
        && marks.every((mark, i) => i === 0 || text.indexOf(marks[i - 1]) < text.indexOf(mark));
    expect('layout: summary reads severity, then tags, then title',
        inOrder(summary, ['finding-severity', '(speculative)', '(preexisting)', 'FULLPROBLEM']));
    expect('layout: body reads full problem, then severity reasoning, then locations',
        inOrder(body, ['FULLPROBLEM', 'REASONMARK', '<code>loc.c:4</code>']));
}

function findingChecks(fns) {
    const failed = [];
    const expect = (name, cond) => { if (!cond) failed.push(name); };
    const groups = [findingTitleChecks, findingOrderChecks, findingCardChecks, findingMalformedChecks,
                    findingEscapingChecks, findingLocationChecks, findingCalibrationLabelChecks,
                    findingCalibrationShapeChecks, findingCalibrationProseChecks,
                    findingCalibrationRenderChecks, findingSpeculativeChecks,
                    findingToggleChecks, findingLocationTrimChecks, findingLineNumberChecks,
                    findingBlankSeverityChecks, findingRowLayoutChecks];
    for (const group of groups) {
        try { group(fns, expect); } catch (e) { failed.push(`${group.name}: threw ${e}`); }
    }
    return failed;
}
console.log('findings on a review card');
{
    const findingFailures = findingChecks(ctx);
    check('every finding check passes', findingFailures.length === 0, findingFailures.join(', '));
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

    // A review that did not finish keeps what it streamed as its final log,
    // headed by a system message and with each entry's stage. That finished
    // log is drawn with the stage marks the live view had, and is not polled.
    timers.clear();
    const header = { role: 'system',
        content: '[The review did not finish. This is the conversation streamed while it ran.]' };
    const promoted = await logPage(ctx, { id: 42, status: 'Failed',
        logs: JSON.stringify([header, ...withSeq(MESSAGES)]) }) || '';
    check('an unfinished review\'s log shows its header', promoted.includes('The review did not finish'));
    check('and marks every staged entry, as the live view does',
        promoted.split('class="log-stage"').length - 1 === MESSAGES.length - 1
        && promoted.includes('>Goal Analysis<'), promoted.slice(0, 600));
    check('and is not polled', pending() === 0, String(pending()));

    // Both writers of a stored log store one shape: each message with its
    // stage and label, none of the stream's seq or created_at, and no stage
    // fields on a message that belongs to no stage. A stored log in that
    // shape is drawn exactly as the live view drew the same conversation, and
    // a promoted log an older build stored with the stream's fields is drawn
    // the same way.
    timers.clear();
    const storedShape = msgs => withSeq(msgs).map(({ seq, created_at, stage, stage_label, ...m }) =>
        (stage == null ? m : { stage, stage_label, ...m }));
    const liveView = await logPage(ctx, { id: 42, status: 'In Review', logs: null,
        live_log: withSeq(MESSAGES) }) || '';
    ctx.stopLogPolling();
    timers.clear();
    const storedView = await logPage(ctx, { id: 42, status: 'Reviewed',
        logs: JSON.stringify(storedShape(MESSAGES)) }) || '';
    check('a stored log is drawn as the live view drew it', storedView !== '' && storedView === liveView,
        storedView.slice(0, 600));
    check('and is not polled', pending() === 0, String(pending()));
    const olderPromoted = await logPage(ctx, { id: 42, status: 'Failed',
        logs: JSON.stringify(withSeq(MESSAGES)) }) || '';
    check('a log stored with the stream\'s fields by an older build is drawn the same',
        olderPromoted === storedView, olderPromoted.slice(0, 600));

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
    for (const status of ['In Review', 'Pending', 'Reviewed', 'Partial', 'Failed']) {
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
    // The per-reference count is on the "Tool calls:" line, from the same rows
    // as the live figure. A read count the record still carries must not be
    // shown beside it as a second, possibly different, count.
    const shown = html => html.replace(/title="[^"]*"/g, '');
    expect('present: the line shows no read count', !/\bread/.test(shown(line))
        && !shown(line).includes('34'));
    // A reference the patch was outside of is kept in the record, not shown:
    // named on every such patch it was noise.
    expect('present: dormant references are not named', !line.includes('iproute2')
        && !line.includes('Not attached') && !line.includes('ip/**'));
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

    // reads: whether or not the record carries them, the hover says nothing
    // about them, since the line no longer shows any
    const titles = html => [...html.matchAll(/title="([^"]*)"/g)].map(m => m[1]).join(' ');
    const early = render({ attached: [{ name: 'kernel', kind: 'kernel', revision: 'v6.18',
        commit: SHA, why: 'always' }] });
    expect('reads: the title says nothing about reads', early.includes('kernel @ v6.18')
        && titles(early) !== '' && !/\bread/.test(titles(early))
        && titles(line) !== '' && !/\bread/.test(titles(line)));

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
    expect('absent: only dormant references render nothing',
        render({ attached: [], dormant: [{ name: 'rdma-core', applies_to: ['rdma/drv/**'] }] }) === '');
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
    const SHORT_COMMIT = '(${escapeHtml(commit.slice(0, 12))})`;';
    const stillReads = mutate('renderReviewReferences', SHORT_COMMIT,
        '(${escapeHtml(commit.slice(0, 12))}), ${a.reads} reads`;');
    check('a References line that still shows reads is caught', referenceChecks(stillReads).length > 0);

    const toolCallMutations = [
        ['a breakdown that drops zero references is caught',
            mutate('toolCallBreakdown', 'const references = split.references',
                'const references = split.references.filter(r => r && r.calls > 0)')],
        ['a breakdown shown for a review with no call is caught',
            mutate('toolCallBreakdown', 'if (!isCallCount(counts.total) || counts.total === 0) return',
                'if (!isCallCount(counts.total)) return')],
        ['a breakdown that does not escape the name is caught',
            mutate('toolCallBreakdown', '${escapeHtml(r.name)}', '${r.name}')],
        ['an unattached clause shown at zero is caught',
            mutate('toolCallBreakdown', "!isCallCount(n) || n === 0 ? ''", "!isCallCount(n) ? ''")],
        ['an attempts note for a run of one attempt is caught',
            mutate('toolCallsText', 'attempts > 1', 'attempts >= 1')],
        ['an attempts note that never shows is caught',
            mutate('toolCallsText', 'Number.isSafeInteger(attempts) && attempts > 1', 'false')],
        ['a card that drops the on-this-attempt marker is caught',
            mutate('reviewToolCallsValue', 'r.attempt > 1', 'false')],
        ['a card that marks a first attempt is caught',
            mutate('reviewToolCallsValue', 'r.attempt > 1', 'r.attempt >= 1')],
        ['a card without the tool-call line is caught',
            mutate('renderReviewCard', '<strong>Tool calls:</strong> ${reviewToolCallsValue(r)}', '')],
        ['a tool-call line without its explanation is caught',
            mutate('renderReviewCard', ' title="${escapeHtml(toolCallsTitle())}"', '')],
        ['a recorded stage table that repeats the counts is caught',
            mutate('renderReviewCard', '${parts.summaryHtml}</summary>',
                '${parts.summaryHtml} — ${toolCallsText(r.tool_calls, 1)}</summary>')],
        ['a recorded stage table passed the counts is caught',
            mutate('renderReviewCard', 'stageTableParts(recordedRows, false, null)',
                'stageTableParts(recordedRows, false, r.tool_calls)')],
        ['a stage table that ignores its counts is caught',
            mutate('stageTableParts', 'const calls = toolCalls &&', 'const calls = null &&')],
        ['a stage table that drops the run\'s attempts is caught',
            mutate('stageTableParts', 'toolCallsText(toolCalls, toolCalls.attempts)', 'toolCallsText(toolCalls, 1)')],
        ['a persisted stage table that drops the counts is caught',
            mutate('stageTableParts', 'const calls = toolCalls &&', 'const calls = live && toolCalls &&')],
        ['a live title that does not explain the counts is caught',
            mutate('paintStageProgress', 'summary.title = parts.showsToolCalls', 'summary.title = false')],
        ['a live title that explains counts that are not there is caught',
            mutate('paintStageProgress', 'summary.title = parts.showsToolCalls', 'summary.title = true')],
        ['a poll that paints every host without counts is caught',
            mutate('refreshActivity', 'toolCalls.get(pid) || null', 'null')],
        ['a poll that keys the counts by review is caught',
            load({ ...sources, toolCallsByPatch: sources.toolCallsByPatch.replaceAll('c.patch_id', 'c.review_id') })],
        ['a poll that trusts tool_calls to be a list is caught',
            mutate('toolCallsByPatch', 'if (!Array.isArray(list)) return new Map();', '')],
        ['a poll that trusts each tool_calls entry to be an object is caught',
            mutate('toolCallsByPatch', "c && typeof c === 'object' && ", '')],
    ];
    for (const [name, broken] of toolCallMutations) {
        check(name, (await toolCallChecks(broken)).length > 0);
    }

    const rowUnescaped = load({ ...sources,
        renderStageRow: sources.renderStageRow.replaceAll('escapeHtml(', 'String(') });
    check('a stage row that does not escape is caught', stageChecks(rowUnescaped).length > 0);
    const oneLabel = load({ ...sources, stageStopLabel: replaceBody('stageStopLabel', " return 'failed';") });
    check('a label that ignores the cause is caught', causeChecks(oneLabel).length > 0);
    const timeAsDone = mutate('stageRowsFromRecord', 'if (st.stopped_by != null) {', 'if (false) {');
    check('a reader that shows a stopped duration as done is caught',
        stoppedTimeChecks(timeAsDone).length > 0);
    const noTime = mutate('stageRowsFromRecord', '...timeOf(time),', '');
    check('a reader that drops a stopped stage\'s time is caught',
        stoppedTimeChecks(noTime).length > 0);
    const noLegacy = load({ ...sources, stageStopCause: replaceBody('stageStopCause',
        " return record.cause != null ? String(record.cause) : 'error';") });
    check('a reader that ignores the legacy flag is caught', causeChecks(noLegacy).length > 0);
    const byName = load({ ...sources,
        compareStageRows: replaceBody('compareStageRows', ' return (a.stage || 0) - (b.stage || 0);') });
    check('a comparator subtracting stage names is caught', stageChecks(byName).length > 0);
    const byInfinity = load({ ...sources, compareStageRows: replaceBody('compareStageRows',
        ' return (a.order ?? Infinity) - (b.order ?? Infinity);') });
    check('a comparator subtracting missing ranks is caught', stageChecks(byInfinity).length > 0);

    const checkForPartial = mutate('renderFindingsBadges',
        "if (status === 'Reviewed' || status === 'Finished') {", "if (status !== 'Pending') {");
    check('a check mark for a Partial review is caught', partialChecks(checkForPartial).length > 0);
    const partialInProgress = mutate('patchSummaryStatus',
        "const partial = reviews.filter(r => r.status === 'Partial');", 'const partial = [];');
    check('a summary that ignores Partial reviews is caught', partialChecks(partialInProgress).length > 0);
    const partialRunCounted = mutate('patchSummaryStatus', "maxOfRunsNote(reviews, 'Partial')",
        "maxOfRunsNote(reviews, null)");
    check('a runs note that counts a Partial run left out of the badges is caught',
        partialChecks(partialRunCounted).length > 0);
    const partialNoRuns = mutate('patchSummaryStatus', "+ maxOfRunsNote(reviews, 'Reviewed')", '');
    check('a Partial summary without the runs note is caught', partialChecks(partialNoRuns).length > 0);
    const partialHidden = mutate('renderReviewCard', '|| partial;', '|| false;');
    check('a card that hides the findings of a Partial review is caught', partialChecks(partialHidden).length > 0);
    const partialSucceeded = mutate('renderReviewCard', 'failed || partial ?', 'failed ?');
    check('a Partial card that says it succeeded is caught', partialChecks(partialSucceeded).length > 0);
    const OPEN_RULE = 'parsedFindings && !foundNothing && !findingsHtml';
    const openOnNothing = mutate('renderReviewCard', OPEN_RULE, 'parsedFindings && !findingsHtml');
    check('a Partial card that opens an empty inline review is caught',
        partialChecks(openOnNothing).length > 0);

    const findingMutations = [
        ['a list ranked by index only is caught',
            mutate('renderFindingsList', 'a.rank - b.rank || a.index - b.index', 'a.index - b.index')],
        ['a title that is never clipped is caught',
            mutate('findingTitle', 'chars.length > FINDING_TITLE_MAX_CHARS', 'false')],
        ['a finding row that does not escape is caught',
            load({ ...sources, renderFindingRow: sources.renderFindingRow.replaceAll('escapeHtml(', 'String(') })],
        ['a location list that does not escape is caught',
            load({ ...sources,
                renderFindingLocations: sources.renderFindingLocations.replaceAll('escapeHtml(', 'String(') })],
        ['a severity colour built from the model\'s text is caught',
            mutate('renderFindingRow', '${getSeverityColor(severity)}',
                'var(--sev-${String(severity).toLowerCase()})')],
        ['an inline review opened beside the findings is caught',
            mutate('renderReviewCard', OPEN_RULE, 'parsedFindings && !foundNothing')],
        ['a list that ignores includePreexisting is caught',
            mutate('renderFindingsList', 'includePreexisting || !f.preexisting', 'true')],
        ['a list built outside the parsedFindings guard is caught',
            mutate('renderReviewCard', 'const findingsHtml = parsedFindings',
                'const findingsHtml = outputJson && Array.isArray(outputJson.findings)')],
        ['a finding with no body that still opens is caught',
            mutate('renderFindingRow', 'if (!body) return', 'if (false) return')],
        ['a preexisting tag on any truthy value is caught',
            mutate('findingTags', 'f.preexisting === true', 'f.preexisting')],
        ['a card without the findings list is caught',
            mutate('renderReviewCard', '${findingsHtml}', '')],
        ['a finding body without the severity reasoning is caught',
            mutate('renderFindingRow', '+ renderSeverityCalibration(f.severity_explanation)', "+ ''")],
        ['a severity reasoning that is not split into its steps is caught',
            mutate('renderSeverityCalibration', 'parseSeverityCalibration(text)', "{ preamble: String(text), sections: [] }")],
        ['a label match without the list-number boundary is caught',
            mutate('parseSeverityCalibration', "const listNumber = '\\\\d+[.)][ \\\\t]+';",
                "const listNumber = '(?!)';")],
        ['a label match that drops the list number after a sentence end is caught',
            mutate('parseSeverityCalibration', '[.;)][ \\\\t]+(?:${listNumber})?)', '[.;)][ \\\\t]+)')],
        ['a label match without the qualifier is caught',
            mutate('parseSeverityCalibration', 'const full = `((${labels})${qualifier})`', 'const full = `((${labels}))`')],
        ['a body that loses its closing parenthesis is caught',
            mutate('parseSeverityCalibration', "m[1].startsWith(')') ? 1 : 0", '0')],
        ['a label match that ignores case is caught',
            mutate('parseSeverityCalibration', "new RegExp(pattern, 'g')", "new RegExp(pattern, 'gi')")],
        ['a label match without the boundary is caught',
            mutate('parseSeverityCalibration', '|[.;)][ \\\\t]+(?:${listNumber})?)', '|)')],
        ['a label match that accepts the half-bold form is caught',
            mutate('parseSeverityCalibration', '(?::\\\\*\\\\*|\\\\*\\\\*:)|${full}:)', '(?::\\\\*\\\\*|\\\\*\\\\*:)?|${full}:)')],
        ['a label match that lets every occurrence open a section is caught',
            mutate('parseSeverityCalibration', 'if (seen.has(base)) continue;', '')],
        ['a label match that keeps the preamble out is caught',
            mutate('parseSeverityCalibration', 'whole.slice(0, marks[0].start).trim() : whole', "'' : whole")],
        ['a label match that keeps the bold in the label is caught',
            mutate('parseSeverityCalibration', 'label: m[2] ?? m[4]', 'label: m[0]')],
        ['a speculative tag read from the severity reasoning is caught',
            mutate('findingTags', 'f.speculative === true', '/speculative/.test(f.severity_explanation)')],
        ['a speculative tag on any truthy value is caught',
            mutate('findingTags', 'f.speculative === true', 'f.speculative')],
        ['a not-marked note shown on every row is caught',
            mutate('renderFindingsList', '${notMarked}${listed.map(({ f }) => renderFindingRow(f)).join(\'\')}',
                '${listed.map(({ f }) => notMarked + renderFindingRow(f)).join(\'\')}')],
        ['a not-marked note that never shows is caught',
            mutate('renderFindingsList', "typeof f.speculative !== 'boolean'", 'false')],
        ['a not-marked note that shows beside marked findings is caught',
            mutate('renderFindingsList', "listed.every(({ f }) => typeof f.speculative !== 'boolean')", 'true')],
        ['a not-marked note that counts any value as marked is caught',
            mutate('renderFindingsList', "typeof f.speculative !== 'boolean'", 'f.speculative === undefined')],
        ['a not-marked note that looks at unlisted findings is caught',
            mutate('renderFindingsList', 'listed.every(({ f }) =>', 'findings.every(f =>')],
        ['a title clipped at exactly the limit is caught',
            mutate('findingTitle', 'chars.length > FINDING_TITLE_MAX_CHARS',
                'chars.length >= FINDING_TITLE_MAX_CHARS')],
        ['a fractional line number shown as a line is caught',
            mutate('renderFindingLocations', 'Number.isSafeInteger(loc.line)', "typeof loc.line === 'number'")],
        ['a line number that is only an integer, not a safe one, is caught',
            mutate('renderFindingLocations', 'Number.isSafeInteger(loc.line)', 'Number.isInteger(loc.line)')],
        ['a Findings button that toggles another id is caught',
            mutate('renderFindingsList', "toggleCollapsible('findings-${uniqueId}'",
                "toggleCollapsible('findings-${uniqueId}-x'")],
        ['a Findings button that toggles no id is caught',
            mutate('renderFindingsList', "toggleCollapsible('findings-${uniqueId}'", "toggleCollapsible('findings-'")],
        ['a location that does not trim its parts is caught',
            mutate('renderFindingLocations', "loc[key].trim() : ''", "loc[key] : ''")],
        ['a severity that is trimmed for the row but not for the counts is caught',
            mutate('renderFindingRow', "f.severity.trim() !== ''\n                ? f.severity : 'Low'",
                "f.severity.trim() !== ''\n                ? f.severity.trim() : 'Low'")],
        ['a blank severity that is not read as Low is caught',
            mutate('renderFindingRow', "f.severity.trim() !== ''", "f.severity !== ''")],
        ['a row with the reasoning after the locations is caught',
            mutate('renderFindingRow',
                '+ renderSeverityCalibration(f.severity_explanation)\n                + renderFindingLocations(f.locations)',
                '+ renderFindingLocations(f.locations)\n                + renderSeverityCalibration(f.severity_explanation)')],
        ['a row with the full problem after the reasoning is caught',
            mutate('renderFindingRow',
                "const body = (clipped ? `<div class=\"finding-text\">${escapeHtml(f.problem.trim())}</div>` : '')\n                + renderSeverityCalibration(f.severity_explanation)",
                "const body = renderSeverityCalibration(f.severity_explanation)\n                + (clipped ? `<div class=\"finding-text\">${escapeHtml(f.problem.trim())}</div>` : '')")],
        ['a row with the title before the tags is caught',
            mutate('renderFindingRow',
                "+ findingTags(f).map(tag => `${tag} `).join('')\n                + `<span title=\"${escapeHtml(title)}\">${escapeHtml(title)}</span>`;",
                "+ `<span title=\"${escapeHtml(title)}\">${escapeHtml(title)}</span>`\n                + findingTags(f).map(tag => ` ${tag}`).join('');")],
        ['a row with the severity after the title is caught',
            mutate('renderFindingRow',
                "const head = `<span class=\"finding-severity\" style=\"color:${getSeverityColor(severity)};\">`\n                + `${escapeHtml(severity)}</span>`\n                + findingTags(f).map(tag => `${tag} `).join('')\n                + `<span title=\"${escapeHtml(title)}\">${escapeHtml(title)}</span>`;",
                "const head = findingTags(f).map(tag => `${tag} `).join('')\n                + `<span title=\"${escapeHtml(title)}\">${escapeHtml(title)}</span>`\n                + `<span class=\"finding-severity\" style=\"color:${getSeverityColor(severity)};\">`\n                + `${escapeHtml(severity)}</span>`;")],
    ];
    for (const [name, broken] of findingMutations) {
        check(name, findingChecks(broken).length > 0);
    }
    const noPartialCard = mutate('renderReviewStatCards',
        "+ renderStatCard('Total Partial', reviewsData.total_partial ?? 0)", '');
    check('a stats page without the Partial card is caught', partialChecks(noPartialCard).length > 0);
    const cutShortDone = mutate('stageRowsFromActivity', "phase.cut_short === true", 'false');
    check('a live cut-short stage shown as done is caught', stoppedTimeChecks(cutShortDone).length > 0);

    const ownRenderer = mutate('renderLogView', '${renderInteractionLogEntries(logs)}',
        "${isLive ? logs.map(e => '<div>' + escapeHtml(e.content || '') + '</div>').join('')"
        + " : renderInteractionLogEntries(logs)}");
    check('a live log drawn by its own renderer is caught', (await logChecks(ownRenderer)).length > 0);
    ownRenderer.stopLogPolling();
}

console.log(failures ? `\n${failures} FAILURE(S)` : '\nALL CHECKS PASSED');
process.exit(failures ? 1 : 0);
