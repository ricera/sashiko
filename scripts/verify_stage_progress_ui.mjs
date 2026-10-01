// Checks the review card in the web UI and the line naming the reference
// repositories each review had. More checks -- the live stage table, activity
// routing, the findings table -- come back as the features they cover are
// ported.
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

const NAMES = ['escapeHtml', 'renderReviewReferences', 'renderReviewCard'];

// Defines the named functions in a scope of their own and returns them, so a
// test can build a variant with one function's source replaced and check that
// the suite notices.
function load(sources) {
    const ctx = {};
    new Function('ctx', NAMES.map(n => sources[n]).join('\n\n') +
        `\n;` + NAMES.map(n => `ctx.${n} = ${n};`).join(''))(ctx);
    return ctx;
}
const sources = Object.fromEntries(NAMES.map(n => [n, grab(n)]));
const ctx = load(sources);

// ---- minimal DOM ---------------------------------------------------------
// The render functions build strings and touch the DOM only through event
// handlers written into them, which never run here. This is enough for a stray
// lookup at render time to come back empty rather than throw.
class El {
    constructor(id, tag = 'div') {
        this.id = id; this.tag = tag; this.style = {}; this.children = [];
        this.innerHTML = ''; this.textContent = ''; this.title = ''; this.className = '';
    }
    appendChild(node) { this.children.push(node); }
}
globalThis.document = {
    getElementById: () => null,
    querySelectorAll: () => [],
    querySelector: () => null,
    createElement: tag => new El(null, tag),
};

let failures = 0;
const check = (name, cond, detail = '') => {
    if (cond) { console.log(`  ok   ${name}`); }
    else { failures++; console.log(`  FAIL ${name}${detail ? ' :: ' + detail : ''}`); }
};

// ---- the review card -----------------------------------------------------
console.log('review card');
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

// ---- reference repositories ------------------------------------------------
const SHA = '1a2b3c4d5e6f7a8b9c0d1a2b3c4d5e6f7a8b9c0d';
const RDMA_SHA = '9f8e7d6c5b4a39281706f5e4d3c2b1a098765432';
const finished = {
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
    const line = render(finished);
    expect('present: labelled', line.includes('References:'));
    expect('present: name and revision', line.includes('kernel @ v6.18'));
    expect('present: short commit', line.includes(`(${SHA.slice(0, 12)})`)
        && line.includes(`(${RDMA_SHA.slice(0, 12)})`));
    expect('present: read count', line.includes('34 reads'));
    expect('present: one read is singular', line.includes('1 read') && !line.includes('1 reads'));
    expect('present: dormant note', line.includes('Not attached: iproute2')
        && line.includes('ip/**'));
    expect('present: shown on the card', card({ id: 1, status: 'Reviewed',
        reference_repos: finished }).includes('kernel @ v6.18'));
    expect('present: shown while running', card({ id: 1, status: 'In Review',
        reference_repos: finished }).includes('References:'));
    expect('present: a JSON string is accepted',
        render(JSON.stringify(finished)).includes('kernel @ v6.18'));

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

// The checks above are only worth something if they fail on a broken renderer.
// Build two from the page's own source and make sure they are caught.
const replaceBody = (name, body) => {
    const fn = sources[name];
    const open = fn.indexOf('{');
    return fn.slice(0, open + 1) + body + '\n        }';
};
const unescaped = load({ ...sources,
    renderReviewReferences: sources.renderReviewReferences.replaceAll('escapeHtml(', 'String(') });
const empty = load({ ...sources,
    renderReviewReferences: replaceBody('renderReviewReferences', " return '';") });
const unwired = load({ ...sources,
    renderReviewCard: sources.renderReviewCard.replace('${referencesHtml}', '') });
check('a renderer that does not escape is caught', referenceChecks(unescaped).length > 0);
check('a renderer that returns nothing is caught', referenceChecks(empty).length > 0);
check('a card that drops the line is caught', referenceChecks(unwired).length > 0);

console.log(failures ? `\n${failures} FAILURE(S)` : '\nALL CHECKS PASSED');
process.exit(failures ? 1 : 0);
