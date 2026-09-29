// Contract tests for web/mill.js. Each test mounts the mill against a fake
// packer whose calls wait until the test answers them, drives it through the
// DOM, and checks what the mill shows. Run by `cargo xtask ui-test`.

import { human, mountMill } from '/web/mill.js';
import { AssertionError, equal, flush, match, ok, run, same, show, sleep, test, waitFor } from './harness.js';

const SETTINGS_KEY = 'pulp.mill.settings';
const PATH_KEY = 'pulp.mill.path';
const stage = document.getElementById('stage');

/* ---------- browser stand-ins ---------- */

const clipboard = [];
const opened = [];
/** Downloads as { name, text }, where text is a promise of the file's contents. */
const saved = [];
Object.defineProperty(navigator, 'clipboard', {
  configurable: true,
  value: {
    async writeText(text) {
      clipboard.push(String(text));
    },
    async write(items) {
      for (const item of items) clipboard.push(await (await item.getType('text/plain')).text());
    },
  },
});
window.open = (url) => {
  opened.push(String(url));
  return null;
};
// Download clicks a temporary a[download]; read its blob instead of saving a file.
document.addEventListener('click', (e) => {
  const link = e.target instanceof Element ? e.target.closest('a[download]') : null;
  if (!link) return;
  e.preventDefault();
  saved.push({ name: link.download, text: fetch(link.href).then((res) => res.text()) });
}, true);

/* ---------- fixtures ---------- */

function file(relative, language, size, extra) {
  return Object.assign({ id: relative, relative, language, kind: 'text', size, default_on: true, oversized: false }, extra);
}

const FILES = [
  file('Cargo.lock', 'toml', 149, { default_on: false }),
  file('Cargo.toml', 'toml', 76),
  file('README.md', 'markdown', 649),
  file('data/archive.zip', 'zip', 171, { kind: 'zip', default_on: false }),
  file('data/big.csv', 'csv', 12 * 1024 * 1024, { kind: 'csv', oversized: true }),
  file('data/readings.csv', 'csv', 106, { kind: 'csv' }),
  file('docs/briefing.docx', 'docx', 999, { kind: 'docx' }),
  file('src/lib.rs', 'rust', 674),
  file('src/main.rs', 'rust', 344),
  file('src/net/mod.rs', 'rust', 120),
];
const TICKED = FILES.filter((f) => f.default_on && !f.oversized).map((f) => f.id);
const TICKED_BYTES = FILES.filter((f) => TICKED.includes(f.id)).reduce((sum, f) => sum + f.size, 0);
const RUST = ['src/lib.rs', 'src/main.rs', 'src/net/mod.rs'];
// Files a hidden-files scan adds.
const HIDDEN = [
  file('.github/ci.yml', 'yaml', 300),
  file('.env.local', 'dotenv', 20, { default_on: false }),
];

/** `dirs` folders of `each` text files: d0/f000.txt, d0/f001.txt, and so on. */
function bigTree(dirs, each) {
  const files = [];
  for (let d = 0; d < dirs; d++) {
    for (let k = 0; k < each; k++) files.push(file('d' + d + '/f' + String(k).padStart(3, '0') + '.txt', 'text', 100));
  }
  return files;
}

const DUMP = '<documents>\n<document index="1">\n<source>README.md</source>\n</document>\n</documents>\n';
const DUMP_LINES = DUMP.split('\n').slice(0, -1);

function packResult(extra) {
  return Object.assign({
    dump: DUMP,
    filename: 'pulp.xml',
    dumpBytes: DUMP.length,
    filesExtracted: TICKED.length,
    filesSkipped: 0,
    tokens: 1234,
    elapsedMs: 12,
    resultId: 'r1',
    outcomes: [],
  }, extra);
}

const ARCHIVE_ISSUE = {
  id: 'data/archive.zip',
  relative: 'data/archive.zip',
  status: 'skipped_archive',
  message: 'nested archive; turn on archives to unpack it',
  kind: 'zip',
  language: 'zip',
  size: 171,
};

/** The local mill's answer while another pack or preview holds its one slot. */
function busyError() {
  return Object.assign(new Error('Another pack or preview is running.'), { busy: true });
}

/* ---------- fake packer ---------- */

/**
 * An adapter whose calls wait for the test: `await a.next('scan')` returns the
 * oldest unanswered scan call, with its `args`, to `resolve` or `reject`.
 * Methods in `a.auto` answer at once instead; cancel does by default.
 */
function fakeAdapter(caps) {
  const calls = {};
  const queued = {};
  const waiters = {};
  const auto = { cancel: () => undefined };
  const call = (method) => (...args) => {
    (calls[method] = calls[method] || []).push(args);
    if (auto[method]) return Promise.resolve().then(() => auto[method](...args));
    const d = { args };
    d.promise = new Promise((resolve, reject) => {
      d.resolve = resolve;
      d.reject = reject;
    });
    (queued[method] = queued[method] || []).push(d);
    const wake = (waiters[method] || []).shift();
    if (wake) wake();
    return d.promise;
  };
  const a = {
    surface: 'local',
    surfaceName: 'test mill',
    version: 'test',
    footer: 'pulp test · 127.0.0.1',
    caps: Object.assign({ source: 'path', gitignore: true, rerender: false, sample: true }, caps),
    auto,
    count: (method) => (calls[method] || []).length,
    next(method, ms = 1500) {
      const queue = (queued[method] = queued[method] || []);
      if (queue.length) return Promise.resolve(queue.shift());
      return new Promise((resolve, reject) => {
        const list = (waiters[method] = waiters[method] || []);
        const wake = () => {
          clearTimeout(timer);
          resolve(queue.shift());
        };
        const timer = setTimeout(() => {
          list.splice(list.indexOf(wake), 1);
          reject(new AssertionError('the mill never called adapter.' + method));
        }, ms);
        list.push(wake);
      });
    },
  };
  for (const method of ['browse', 'pickFiles', 'sample', 'scan', 'pack', 'render', 'preview', 'tree', 'fullDump', 'cancel']) {
    a[method] = call(method);
  }
  return a;
}

/* ---------- mounting and reading the mill ---------- */

// The mill mounted last. The next mount destroys it, so page keys such as
// Ctrl+Enter and Esc reach only the mill under test.
let current = null;

function mount(adapter, { width = 1280, options, keepStorage = false } = {}) {
  if (current) current.destroy();
  if (!keepStorage) {
    localStorage.removeItem(SETTINGS_KEY);
    localStorage.removeItem(PATH_KEY);
  }
  const root = document.createElement('div');
  root.style.width = width + 'px';
  stage.replaceChildren(root);
  const api = mountMill(root, adapter, options);
  current = api;
  return {
    root,
    api,
    $: (selector) => root.querySelector(selector),
    $$: (selector) => [...root.querySelectorAll(selector)],
  };
}

// Read the mill through its data-* hooks and state attributes; wording and
// layout are free to change.
const text = (el) => (el ? el.textContent.trim() : '');
const visible = (el) => !!el && el.getClientRects().length > 0;
/** Disabled, or marked aria-disabled while it stays focusable. */
const unavailable = (el) => el.disabled || el.getAttribute('aria-disabled') === 'true';
const acts = (el) => [...el.querySelectorAll('[data-act]')].map((b) => b.dataset.act);
const status = (m) => text(m.$('[data-el="status"]'));
const tone = (m) => m.$('[data-el="status"]').dataset.tone;
const row = (m, id) => m.$('.mill-row[data-id="' + CSS.escape(id) + '"]');
const fileIds = (m) => m.$$('.mill-row[data-id]').map((r) => r.dataset.id);
/** The files pane's ticked and total counts. */
const counts = (m) => (text(m.$('[data-el="filecount"]')).match(/\d+/g) || []).map(Number);
const box = (m, id) => row(m, id).querySelector('[data-act="check"]');
const ticked = (m, id) => box(m, id).checked;
const toggle = (m, id) => box(m, id).click();
/** The dump's lines; the ruling on an empty sheet is aria-hidden. */
const lines = (m) => m.$$('[data-el="dump"] .ln:not([aria-hidden="true"])');
const lineTexts = (m) => lines(m).map((l) => l.textContent.replace(/\n$/, ''));
const view = (m) => m.$('[data-view][aria-selected="true"]').dataset.view;
const lastToast = (m) => text(m.$$('[data-el="toasts"] > *').pop());
const alertBox = (m) => m.$('[data-el="alert"]');
const pulpKey = (m) => m.$('[data-el="pulpkey"]');
const copyKey = (m) => m.$('.mill-outkeys [data-act="copy"]');
/** Each chip as "language on/total state". */
const chips = (m) => m.$$('[data-act="chip"]').map((c) => c.dataset.lang + ' ' + (text(c).match(/\d+\/\d+/) || ['?'])[0] + ' ' + c.dataset.state);
/** Primary keys a user can see. */
const primaries = (m) => m.$$('.is-primary').filter(visible);
/** Rows the tree has drawn; a large tree draws only those near its viewport. */
const drawn = (m) => m.$$('[data-el="rows"] .mill-row').length;
const dirRow = (m, path) => m.$('.mill-row[data-dir="' + path + '"]');
const focusedRow = () => document.activeElement.closest('.mill-row');

/** Everything Tab can reach in the tree, the rows element included while it holds the stop. */
function tabStops(m) {
  const rows = m.$('[data-el="rows"]');
  return [rows, ...rows.querySelectorAll('button, input, [tabindex]')].filter((c) => c.tabIndex >= 0 && !c.disabled);
}

function press(target, key, init) {
  target.dispatchEvent(new KeyboardEvent('keydown', Object.assign({ key, bubbles: true, cancelable: true }, init)));
}

function typeFilter(m, value) {
  const input = m.$('[data-el="filter"]');
  input.value = value;
  input.dispatchEvent(new Event('input', { bubbles: true }));
}

function typePath(m, value) {
  const input = m.$('[data-el="path"]');
  input.value = value;
  input.dispatchEvent(new Event('input', { bubbles: true }));
  return input;
}

/** Let the page render, as it does while any real request is out. */
async function frames() {
  await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  await flush();
}

/** Mount, and scan `files` from a typed path. */
async function scanned(files, path = '/tmp/project') {
  const a = fakeAdapter();
  const m = mount(a);
  press(typePath(m, path), 'Enter');
  (await a.next('scan')).resolve({ files });
  await flush();
  return { a, m };
}

/**
 * Scan `bigTree(dirs, each)`, which starts with every folder collapsed, and open
 * them all: the last first, so each folder row is still near the top, drawn.
 */
async function expanded(dirs, each) {
  const files = bigTree(dirs, each);
  const { a, m } = await scanned(files);
  for (let d = dirs - 1; d >= 0; d--) dirRow(m, 'd' + d).querySelector('[data-act="twist"]').click();
  return { a, m, total: dirs + files.length };
}

/** Mount, press Try a sample, and answer the sample and its scan. */
async function sampled(caps, width) {
  const a = fakeAdapter(caps);
  const m = mount(a, { width });
  m.$('.mill-empty [data-act="sample"]').click();
  (await a.next('sample')).resolve({ path: '/tmp/tides', sample: true });
  (await a.next('scan')).resolve({ files: FILES });
  await flush();
  return { a, m };
}

/** `sampled`, then Pulp the default ticks and answer with `result`. */
async function pulped(caps, result) {
  const { a, m } = await sampled(caps);
  pulpKey(m).click();
  (await a.next('pack')).resolve(packResult(result));
  await flush();
  return { a, m };
}

/** `sampled`, tick the archive, and Pulp into a result that flags it. */
async function flagged() {
  const { a, m } = await sampled();
  toggle(m, 'data/archive.zip');
  pulpKey(m).click();
  const outcomes = TICKED.map((id) => ({ id, relative: id, status: 'extracted', message: '' })).concat([ARCHIVE_ISSUE]);
  (await a.next('pack')).resolve(packResult({ outcomes }));
  await flush();
  return { a, m };
}

/* ---------- mount ---------- */

test('mount: empty state, Pulp off, footer, no alert', async () => {
  const m = mount(fakeAdapter());
  ok(!m.$('[data-el="filesempty"]').hidden, 'the empty state shows');
  ok(m.$('[data-el="filesbody"]').hidden, 'no file list yet');
  ok(m.$('[data-el="filesloading"]').hidden, 'nothing is loading');
  ok(unavailable(pulpKey(m)), 'Pulp is unavailable with nothing ticked');
  equal(tone(m), 'idle', 'no dump yet');
  ok(unavailable(copyKey(m)), 'Copy is unavailable');
  ok(alertBox(m).hidden, 'no alert');
  match(text(m.$('.mill-foot')), /pulp test · 127\.0\.0\.1/, 'the adapter footer shows');
  match(text(m.$('[data-el="footsel"]')), /no folder/);
  const browse = m.$('.mill-empty [data-act="browse"]');
  ok(browse.classList.contains('is-primary'), 'the empty state carries the primary Browse key');
  ok(!m.$('.mill-source [data-act="browse"]').classList.contains('is-primary'), 'the Browse key by the path is not primary');
  const keys = primaries(m);
  ok(keys.length === 1 && keys[0] === browse, 'one primary key shows: ' + keys.map(show).join(', '));
  ok(m.$('.mill-empty [data-act="sample"]'), 'Try a sample is offered');
  ok(!visible(m.$('[data-el="tabs"]')) && !visible(m.$('[data-el="dock"]')), 'no phone tabs or dock when wide');
  equal(m.root.dataset.surface, 'local');

  const phone = mount(fakeAdapter(), { width: 390 });
  const phoneKeys = primaries(phone);
  ok(phoneKeys.length === 1 && phoneKeys[0].dataset.act === 'browse' && phoneKeys[0].closest('[data-el="dock"]'),
    'on a phone the dock carries the one primary key: ' + phoneKeys.map(show).join(', '));

  const bare = mount(fakeAdapter({ sample: false }));
  equal(bare.$('[data-act="sample"]'), null, 'no sample key without caps.sample');
});

/* ---------- scan ---------- */

test('sample: rows, chips, and counts; default_on and the size cap respected', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  m.$('.mill-empty [data-act="sample"]').click();
  const sample = await a.next('sample');
  equal(m.root.dataset.busy, 'sample');
  ok(!m.$('[data-el="filesloading"]').hidden, 'the loading state shows');
  sample.resolve({ path: '/tmp/tides', sample: true });
  const scan = await a.next('scan');
  same(scan.args[0], { path: '/tmp/tides', gitignore: true, hidden: false, archives: false }, 'scan request');
  equal(m.root.dataset.busy, 'scan');
  ok(unavailable(m.$('.mill-source [data-act="scan"]')), 'source keys are unavailable while scanning');
  scan.resolve({ files: FILES });
  await flush();

  equal(m.root.dataset.busy, '');
  same(fileIds(m).sort(), FILES.map((f) => f.id).sort(), 'one row per file');
  same(m.$$('.mill-row[data-dir]').map((r) => r.dataset.dir).sort(), ['data', 'docs', 'src', 'src/net'], 'one row per folder');
  same(counts(m), [TICKED.length, FILES.length], 'ticked and total');
  ok(!ticked(m, 'Cargo.lock') && row(m, 'Cargo.lock').classList.contains('is-off'), 'a default-off lockfile starts unticked');
  ok(!ticked(m, 'data/archive.zip'), 'a default-off archive starts unticked');
  ok(ticked(m, 'src/lib.rs') && !row(m, 'src/lib.rs').classList.contains('is-off'), 'source starts ticked');
  const big = box(m, 'data/big.csv');
  ok(unavailable(big) && !big.checked, 'an oversized file starts unticked and unavailable');
  big.click();
  ok(!ticked(m, 'data/big.csv'), 'an oversized file cannot be ticked');
  same(counts(m), [TICKED.length, FILES.length], 'the ticks are unchanged');
  same(chips(m), ['rust 3/3 on', 'toml 1/2 mix', 'csv 1/1 on', 'docx 1/1 on', 'markdown 1/1 on', 'zip 0/1 off']);
  match(text(m.$('[data-el="pulptext"]')), new RegExp('\\b' + TICKED.length + ' files\\b'), 'the Pulp key counts the ticks');
  ok(!unavailable(pulpKey(m)) && pulpKey(m).classList.contains('is-primary'), 'Pulp is the primary key');
  const footer = text(m.$('[data-el="footsel"]'));
  match(footer, new RegExp('\\b' + TICKED.length + ' of ' + FILES.length + ' ticked'), 'the footer counts the ticks');
  ok(footer.includes(human(TICKED_BYTES)), 'the footer sums the ticked sizes: ' + footer);
  equal(m.$('[data-el="path"]').value, '/tmp/tides');
  equal(localStorage.getItem(PATH_KEY), null, 'a sample path is not remembered');
});

test('empty: a scan with nothing to pulp says so, offers Browse, and an option rescans it', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  const empty = m.$('[data-el="filesempty"]');
  ok(empty.querySelector('.mill-steps') && !empty.querySelector('.mill-nofiles'), 'a first visit shows the steps');
  press(typePath(m, '/tmp/empty'), 'Enter');
  (await a.next('scan')).resolve({ files: [] });
  await flush();

  ok(!empty.hidden, 'the files pane shows its empty state');
  ok(empty.querySelector('.mill-nofiles'), 'it says there is nothing to pulp');
  equal(empty.querySelector('.mill-steps'), null, 'not the first-visit steps');
  equal(fileIds(m).length, 0, 'no rows');
  const browse = empty.querySelector('[data-act="browse"]');
  ok(browse && visible(browse) && !unavailable(browse), 'a Browse key is offered');
  const keys = primaries(m);
  ok(keys.length === 1 && keys[0] === browse, 'it is the one primary key: ' + keys.map(show).join(', '));
  const foot = text(m.$('[data-el="footsel"]'));
  ok(foot && !/\bno folder\b/.test(foot), 'the footer no longer says no folder: ' + show(foot));
  browse.click();
  (await a.next('browse')).resolve({ cancelled: true });
  await flush();

  const hidden = m.$('[data-opt="hidden"]');
  hidden.checked = true;
  hidden.dispatchEvent(new Event('change', { bubbles: true }));
  const rescan = await a.next('scan');
  same(rescan.args[0], { path: '/tmp/empty', gitignore: true, hidden: true, archives: false }, 'the rescan uses the new setting');
  rescan.resolve({ files: HIDDEN });
  await flush();
  same(fileIds(m).sort(), HIDDEN.map((f) => f.id).sort(), 'the wider scan lists what it found');
  ok(empty.hidden, 'the empty state goes');
});

/* ---------- path field ---------- */

test('path: a typed path scans after the field loses focus, by Scan or by Enter', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  const input = typePath(m, '/tmp/typed');
  input.dispatchEvent(new FocusEvent('blur'));
  equal(input.value, '/tmp/typed', 'blur keeps the typed text');
  m.$('.mill-source [data-act="scan"]').click();
  const scan = await a.next('scan');
  equal(scan.args[0].path, '/tmp/typed', 'Scan takes the typed path');
  ok(alertBox(m).hidden, 'no alert asks for a folder');
  scan.resolve({ files: FILES });
  await flush();
  equal(fileIds(m).length, FILES.length, 'the typed folder is listed');
  equal(localStorage.getItem(PATH_KEY), '"/tmp/typed"', 'a typed path is remembered');

  typePath(m, '/tmp/other').dispatchEvent(new FocusEvent('blur'));
  press(input, 'Enter');
  const again = await a.next('scan');
  equal(again.args[0].path, '/tmp/other', 'Enter after a blur scans the newly typed path');
  again.resolve({ files: FILES });
  await flush();
  equal(localStorage.getItem(PATH_KEY), '"/tmp/other"');
});

test('path: a sample shows its name, and Scan rescans its folder without remembering it', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  m.$('.mill-empty [data-act="sample"]').click();
  (await a.next('sample')).resolve({ path: '/tmp/x/tides', display: 'tides (sample)', sample: true });
  const first = await a.next('scan');
  equal(first.args[0].path, '/tmp/x/tides', 'the sample scans its real folder');
  first.resolve({ files: FILES });
  await flush();
  const input = m.$('[data-el="path"]');
  equal(input.value, 'tides (sample)', 'the field shows the display name');

  input.dispatchEvent(new FocusEvent('blur'));
  m.$('.mill-source [data-act="scan"]').click();
  const again = await a.next('scan');
  equal(again.args[0].path, '/tmp/x/tides', 'Scan with the name unchanged rescans the sample folder');
  again.resolve({ files: FILES });
  await flush();
  equal(input.value, 'tides (sample)', 'the field still shows the name');
  equal(localStorage.getItem(PATH_KEY), null, 'the temp folder is not remembered');

  press(typePath(m, '/home/me/tides'), 'Enter');
  const typed = await a.next('scan');
  equal(typed.args[0].path, '/home/me/tides', 'a typed path replaces the sample');
  typed.resolve({ files: FILES });
  await flush();
  equal(localStorage.getItem(PATH_KEY), '"/home/me/tides"', 'and is remembered');
});

/* ---------- pulp ---------- */

test('pulp: Ready, Copy becomes the primary key, dump lines are numbered', async () => {
  const { a, m } = await sampled();
  pulpKey(m).click();
  const pack = await a.next('pack');
  const req = pack.args[0];
  same(req.selected.slice().sort(), TICKED.slice().sort(), 'packs the ticked files');
  same([req.path, req.format, req.source, req.tree, req.gitignore, req.hidden, req.archives, req.notebook],
    ['/tmp/tides', 'xml', false, true, true, false, false, false], 'pack request settings');
  equal(tone(m), 'busy', 'the status shows the pack');
  ok(!m.$('[data-el="cancelkey"]').hidden, 'Cancel shows while packing');
  ok(unavailable(pulpKey(m)), 'Pulp is unavailable while packing');
  ok(!m.$('[data-el="progress"]').hidden, 'the progress bar shows');
  pulpKey(m).click();
  await flush();
  equal(a.count('pack'), 1, 'a second Pulp does not start another pack');

  pack.resolve(packResult());
  await flush();
  equal(status(m), 'Ready');
  equal(tone(m), 'ok');
  const copy = copyKey(m);
  ok(copy.classList.contains('is-primary') && !unavailable(copy), 'Copy is the primary key');
  ok(!pulpKey(m).classList.contains('is-primary'), 'Pulp is no longer primary');
  ok(!unavailable(m.$('.mill-outkeys [data-act="download"]')), 'Download is available');
  same(lines(m).map((l) => l.dataset.n), ['1', '2', '3', '4', '5'], 'ledger line numbers');
  same(lineTexts(m), DUMP_LINES, 'the dump text shows as is');
  ok(m.$('[data-el="watermark"]').hidden, 'no watermark over a dump');
  ok(m.$('[data-el="cancelkey"]').hidden && m.$('[data-el="progress"]').hidden, 'Cancel and progress hide');
  equal(m.root.dataset.tab, 'output', 'the phone tab moves to the output');
  match(text(m.$('[data-el="statmeta"]')), /1\.2k tokens/, 'the token estimate shows');
});

test('copy, download, tree: the dump and the directory map leave through their keys', async () => {
  ok(window.isSecureContext, 'the clipboard needs a secure context');
  const { a, m } = await pulped();
  const copies = clipboard.length;
  copyKey(m).click();
  await waitFor(() => clipboard.length > copies, { what: 'the Copy write' });
  equal(clipboard.at(-1), DUMP, 'Copy puts the dump on the clipboard');
  match(lastToast(m), /pulp\.xml/, 'the Copy toast names the dump');

  const saves = saved.length;
  m.$('.mill-outkeys [data-act="download"]').click();
  await waitFor(() => saved.length > saves, { what: 'the download' });
  equal(saved.at(-1).name, 'pulp.xml', 'Download saves under the result filename');
  equal(await saved.at(-1).text, DUMP, 'the saved file is the dump');
  match(lastToast(m), /pulp\.xml/, 'the Download toast names the file');

  m.$('.mill-outkeys [data-act="tree"]').click();
  const tree = await a.next('tree');
  equal(tree.args[0].format, 'xml');
  tree.resolve({ text: 'tides/\n└── src/\n', filename: 'pulp-tree.xml' });
  await waitFor(() => clipboard.at(-1) === 'tides/\n└── src/\n', { what: 'Tree to copy the directory map' });
  match(lastToast(m), /directory map/i, 'the Tree toast says what was copied');
  equal(a.count('fullDump'), 0, 'a whole dump needs no second fetch');
});

test('output keys: Copy, Download, and Tree are patched in place through a pulp', async () => {
  const { a, m } = await sampled();
  const keys = () => ['copy', 'download', 'tree'].map((act) => m.$('.mill-outkeys [data-act="' + act + '"]'));
  const before = keys();
  before[0].focus();
  pulpKey(m).click();
  (await a.next('pack')).resolve(packResult());
  await flush();
  equal(status(m), 'Ready');
  keys().forEach((key, j) => ok(key === before[j], before[j].dataset.act + ' is the same element, not a redrawn one'));
  ok(!unavailable(before[0]) && before[0].classList.contains('is-primary'), 'Copy is ready');
  equal(document.activeElement, before[0], 'a focused Copy key keeps its focus, got ' + show(document.activeElement));
});

test('truncated: a clipped dump says so, and Copy and Download fetch the whole dump', async () => {
  const { a, m } = await pulped(undefined, { previewTruncated: true, dumpBytes: 5 * 1024 * 1024 });
  const note = m.$('[data-el="outnote"]');
  ok(!note.hidden, 'the clipped-dump note shows');
  match(text(note), /5\.0 MiB/, 'the note gives the full size');

  copyKey(m).click();
  const forCopy = await a.next('fullDump');
  equal(forCopy.args[0].resultId, 'r1');
  forCopy.resolve('THE WHOLE DUMP');
  await waitFor(() => clipboard.at(-1) === 'THE WHOLE DUMP', { what: () => 'the whole dump on the clipboard, got ' + show(clipboard.at(-1)) });

  const saves = saved.length;
  m.$('.mill-outkeys [data-act="download"]').click();
  (await a.next('fullDump')).resolve('THE WHOLE DUMP');
  await waitFor(() => saved.length > saves, { what: 'the download' });
  equal(await saved.at(-1).text, 'THE WHOLE DUMP', 'the saved file is the whole dump');
});

test('progress: files read show while the packer reports them', async () => {
  const { a, m } = await sampled({ progress: true });
  pulpKey(m).click();
  const pack = await a.next('pack');
  const hooks = pack.args[1];
  ok(hooks && typeof hooks.onProgress === 'function', 'pack gets an onProgress hook');
  match(text(m.$('[data-el="why"]')), new RegExp('\\b0 of ' + TICKED.length + '\\b'), 'progress starts at zero');
  hooks.onProgress(3, 7);
  match(text(m.$('[data-el="why"]')), /\b3 of 7\b/, 'progress follows the packer');
  const bar = m.$('[data-el="progress"]');
  ok('p' in bar.dataset, 'the bar shows measured progress');
  equal(bar.style.getPropertyValue('--p'), String(3 / 7));
  pack.resolve(packResult());
  await flush();
  ok(bar.hidden, 'the bar hides when the pack is done');
});

/* ---------- out of date ---------- */

test('stale: a tick change marks the dump out of date until the ticks match again', async () => {
  const { m } = await pulped();
  toggle(m, 'README.md');
  equal(status(m), 'Out of date');
  equal(tone(m), 'stale');
  ok(unavailable(copyKey(m)) && !copyKey(m).classList.contains('is-primary'), 'Copy is unavailable');
  ok(unavailable(m.$('.mill-outkeys [data-act="download"]')), 'Download is unavailable');
  const note = m.$('[data-el="outnote"]');
  ok(!note.hidden, 'the out-of-date note shows');
  match(text(note.querySelector('[data-act="pulp"]')), /pulp again/i, 'the note offers Pulp again');
  ok(pulpKey(m).classList.contains('is-primary'), 'Pulp is the primary key again');
  ok(m.$('[data-el="dump"]').classList.contains('is-stale'), 'the old dump is dimmed');

  toggle(m, 'README.md');
  equal(status(m), 'Ready', 'the same ticks match the dump again');
  ok(note.hidden && !unavailable(copyKey(m)), 'the note goes and Copy comes back');
});

test('rerender: a format or directory-map change redraws without a new pack', async () => {
  const { a, m } = await pulped({ rerender: true });
  m.$('input[data-setting="format"][value="md"]').click();
  const render = await a.next('render');
  equal(render.args[0].format, 'md');
  equal(tone(m), 'busy');
  match(status(m), /redraw/i, 'the status says it is a redraw, not a pack');
  render.resolve(packResult({ dump: '## README.md\n', filename: 'pulp.md' }));
  await flush();
  equal(status(m), 'Ready');
  same(lineTexts(m), ['## README.md'], 'the redrawn dump shows');

  m.$('[data-opt="tree"]').click();
  const noMap = await a.next('render');
  equal(noMap.args[0].tree, false, 'the directory map setting redraws too');
  noMap.resolve(packResult({ dump: 'no map\n', filename: 'pulp.md' }));
  await flush();
  equal(status(m), 'Ready');
  equal(a.count('pack'), 1, 'no new pack for a redraw');

  m.$('input[data-setting="content"][value="source"]').click();
  await flush();
  equal(status(m), 'Out of date', 'Source content needs a new extraction');
  equal(a.count('render'), 2, 'a content change does not redraw');
});

test('no rerender: a format change marks the dump out of date', async () => {
  const { a, m } = await pulped({ rerender: false });
  m.$('input[data-setting="format"][value="txt"]').click();
  await flush();
  equal(status(m), 'Out of date');
  equal(a.count('render'), 0, 'render is never called');
  m.$('input[data-setting="format"][value="xml"]').click();
  await flush();
  equal(status(m), 'Ready', 'the original format matches the dump again');
});

/* ---------- discovery options ---------- */

test('discovery: a hidden-files toggle rescans, keeps ticks, and ticks new default-on files', async () => {
  const { a, m } = await sampled();
  toggle(m, 'README.md');
  toggle(m, 'Cargo.lock');
  m.$('.mill-row[data-dir="src"] .mill-twist').click();
  m.$('[data-opt="hidden"]').click();
  const scan = await a.next('scan');
  same(scan.args[0], { path: '/tmp/tides', gitignore: true, hidden: true, archives: false }, 'rescan request');
  scan.resolve({ files: FILES.concat(HIDDEN) });
  await flush();
  ok(!ticked(m, 'README.md'), 'an unticked file stays unticked');
  ok(ticked(m, 'Cargo.lock'), 'a ticked lockfile stays ticked');
  ok(ticked(m, '.github/ci.yml'), 'a new default-on file is ticked');
  ok(!ticked(m, '.env.local'), 'a new default-off file is not');
  same(counts(m), [TICKED.length + 1, FILES.length + HIDDEN.length], 'ticked and total');
  equal(row(m, 'src/lib.rs'), null, 'a collapsed folder stays collapsed');
});

test('discovery: two toggles during a scan replay once with the final settings', async () => {
  const { a, m } = await sampled();
  m.$('[data-opt="hidden"]').click();
  const first = await a.next('scan');
  equal(first.args[0].hidden, true);
  m.$('[data-opt="archives"]').click();
  m.$('[data-opt="hidden"]').click();
  await flush();
  equal(a.count('scan'), 2, 'no scan starts while one is running');

  first.resolve({ files: FILES.concat(HIDDEN) });
  const second = await a.next('scan');
  same(second.args[0], { path: '/tmp/tides', gitignore: true, hidden: false, archives: true }, 'the replay uses the final settings');
  second.resolve({ files: FILES });
  await flush();
  await sleep(30);
  equal(a.count('scan'), 3, 'nothing is left to replay');
  equal(m.root.dataset.busy, '');
  equal(row(m, '.github/ci.yml'), null, 'the tree matches the last scan');

  pulpKey(m).click();
  const pack = await a.next('pack');
  equal(pack.args[0].archives, true);
  equal(pack.args[0].hidden, false);
  pack.resolve(packResult());
  await flush();
  equal(status(m), 'Ready', 'the replayed scan pulps cleanly');
  equal(a.count('scan'), 3, 'Pulp needed no further scan');
});

/* ---------- busy and cancel ---------- */

test('busy: Enter in the path field and Scan do nothing while a pack runs', async () => {
  const { a, m } = await sampled();
  pulpKey(m).click();
  const pack = await a.next('pack');
  const input = m.$('[data-el="path"]');
  input.value = '/elsewhere';
  press(input, 'Enter');
  const scanKey = m.$('.mill-source [data-act="scan"]');
  ok(unavailable(scanKey), 'Scan is unavailable while packing');
  scanKey.click();
  await flush();
  equal(a.count('scan'), 1, 'no scan started');

  pack.resolve(packResult());
  await flush();
  equal(status(m), 'Ready');
  equal(fileIds(m).length, FILES.length, 'the tree is untouched');
  press(input, 'Enter');
  const scan = await a.next('scan');
  equal(scan.args[0].path, '/elsewhere', 'Enter scans once the pack is done');
  scan.resolve({ files: FILES });
  await flush();
});

test('cancel: Cancel keeps the previous dump and says Pulp cancelled', async () => {
  const { a, m } = await pulped();
  toggle(m, 'Cargo.lock');
  pulpKey(m).click();
  const pack = await a.next('pack');
  const cancel = m.$('[data-el="cancelkey"]');
  ok(!cancel.hidden, 'Cancel shows');
  cancel.click();
  await flush();
  equal(a.count('cancel'), 1, 'the adapter is asked to cancel');
  pack.resolve({ cancelled: true });
  await flush();
  match(lastToast(m), /Pulp cancelled/);
  same(lineTexts(m), DUMP_LINES, 'the previous dump stays');
  equal(status(m), 'Out of date', 'the kept dump predates the new tick');
  ok(cancel.hidden && alertBox(m).hidden, 'no Cancel key and no alert afterwards');
  equal(m.root.dataset.busy, '');
});

test('cancel: Esc cancels, and a pack rejected as cancelled is not an error', async () => {
  const { a, m } = await pulped();
  toggle(m, 'Cargo.lock');
  pulpKey(m).click();
  const pack = await a.next('pack');
  press(document.body, 'Escape');
  await flush();
  equal(a.count('cancel'), 1, 'Esc asks the adapter to cancel');
  pack.reject(Object.assign(new Error('cancelled'), { cancelled: true }));
  await flush();
  match(lastToast(m), /Pulp cancelled/);
  ok(alertBox(m).hidden, 'no alert for a cancel');
  equal(status(m), 'Out of date');
});

/* ---------- issues ---------- */

test('issues: flagged rows, a count badge, and Untick or Report on the flagged file', async () => {
  const { a, m } = await flagged();
  equal(tone(m), 'warn');
  match(status(m), /\b1 issue\b/, 'the status counts the issues');
  same(m.$$('.mill-row.is-flag[data-id]').map((r) => r.dataset.id), ['data/archive.zip'], 'the flagged row');
  const badge = m.$('[data-el="issuecount"]');
  ok(!badge.hidden, 'the Issues tab has a badge');
  equal(text(badge), '1');
  const name = row(m, 'data/archive.zip').querySelector('[data-act="preview"]');
  match(name.getAttribute('aria-label'), /flagged/, 'the row tells a screen reader it is flagged');

  m.$('[data-view="issues"]').click();
  equal(view(m), 'issues');
  const issues = m.$$('.mill-issue');
  equal(issues.length, 1, 'one issue listed');
  match(text(issues[0]), /skipped.archive/);

  name.click();
  const preview = await a.next('preview');
  equal(preview.args[0].relative, 'data/archive.zip');
  preview.resolve({ text: '', status: 'skipped_archive', message: 'nested archive' });
  await flush();
  equal(view(m), 'preview');
  ok(!m.$('[data-panel="preview"]').hidden, 'the preview panel shows');
  const note = m.$('[data-el="prevnote"]');
  ok(!note.hidden, 'the flagged file shows its note');
  match(text(note), /skipped.archive/);
  ok(note.querySelector('[data-act="untick"]'), 'Untick is offered');
  ok(note.querySelector('[data-act="archives-on"]'), 'Turn on archives is offered');

  note.querySelector('[data-act="report-issue"]').click();
  const url = new URL(opened.at(-1));
  equal(url.origin + url.pathname, 'https://github.com/BeeGass/pulp/issues/new');
  match(url.searchParams.get('title'), /skipped_archive.*data\/archive\.zip/, 'the issue title names the status and file');
  match(url.searchParams.get('body'), /skipped_archive/);
  match(url.searchParams.get('body'), /nested archive; turn on archives/, 'the report carries the error text');
});

test('issues: Untick on the flagged file, then Pulp again, returns to Combined', async () => {
  const { a, m } = await flagged();
  row(m, 'data/archive.zip').querySelector('[data-act="preview"]').click();
  (await a.next('preview')).resolve({ text: '', status: 'skipped_archive', message: 'nested archive' });
  await flush();
  m.$('[data-el="prevnote"] [data-act="untick"]').click();
  ok(!ticked(m, 'data/archive.zip'), 'Untick unticks the file');
  equal(status(m), 'Out of date');

  m.$('[data-el="outnote"] [data-act="pulp"]').click();
  const again = await a.next('pack');
  ok(!again.args[0].selected.includes('data/archive.zip'), 'the flagged file is left out');
  again.resolve(packResult());
  await flush();
  equal(view(m), 'combined');
  ok(!m.$('[data-panel="combined"]').hidden && m.$('[data-panel="preview"]').hidden, 'the Combined panel shows');
  equal(status(m), 'Ready');
  equal(m.$$('.mill-row.is-flag[data-id]').length, 0, 'no flags left');
  ok(m.$('[data-el="issuecount"]').hidden, 'no issue badge');
});

test('issues: a pack opens the collapsed folders over a flagged file, and a folder closed over it shows the flag', async () => {
  const bad = file('d1/deep/notes.pdf', 'pdf', 999, { kind: 'pdf' });
  const { a, m } = await scanned(bigTree(3, 150).concat([bad]));
  equal(row(m, bad.id), null, 'a large tree starts with its folders collapsed');
  pulpKey(m).click();
  (await a.next('pack')).resolve(packResult({
    outcomes: [{ id: bad.id, relative: bad.relative, status: 'error', message: 'broken cross-reference table' }],
  }));
  await flush();
  equal(tone(m), 'warn');
  ok(row(m, bad.id) && row(m, bad.id).classList.contains('is-flag'), 'the flagged file shows, flagged');
  same(['d1', 'd1/deep'].map((path) => dirRow(m, path).getAttribute('aria-expanded')), ['true', 'true'], 'the folders over it opened');
  equal(dirRow(m, 'd0').getAttribute('aria-expanded'), 'false', 'other folders stay collapsed');
  equal(dirRow(m, 'd1').querySelector('.mill-flag'), null, 'an open folder carries no flag of its own');

  dirRow(m, 'd1').querySelector('[data-act="twist"]').click();
  equal(row(m, bad.id), null, 'the folder closes');
  ok(dirRow(m, 'd1').querySelector('.mill-flag'), 'the closed folder shows the flag');
  match(dirRow(m, 'd1').getAttribute('aria-label'), /flagged/, 'its treeitem label mentions flagged files');
  equal(dirRow(m, 'd0').querySelector('.mill-flag'), null, 'a folder with no flagged files shows none');
  ok(!/flagged/.test(dirRow(m, 'd0').getAttribute('aria-label')), 'nor says so');
});

/* ---------- alerts ---------- */

test('alert: a failed scan shows a reportable alert that a later scan clears', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  const input = m.$('[data-el="path"]');
  input.value = '/no/such/folder';
  press(input, 'Enter');
  const scan = await a.next('scan');
  equal(scan.args[0].path, '/no/such/folder');
  scan.reject(new Error('no such folder: /no/such/folder'));
  await flush();
  const alert = alertBox(m);
  ok(!alert.hidden, 'the alert shows');
  match(text(alert), /scan failed/i);
  match(text(alert), /no such folder: \/no\/such\/folder/, 'the alert carries the error text');
  equal(alert.querySelector('[data-tone]').dataset.tone, 'bad');
  equal(fileIds(m).length, 0, 'no stale tree');
  ok(!m.$('[data-el="filesempty"]').hidden, 'the empty state is back');
  alert.querySelector('[data-act="report-alert"]').click();
  match(new URL(opened.at(-1)).searchParams.get('body'), /no such folder/, 'the report carries the error text');

  input.value = '/tmp/tides';
  press(input, 'Enter');
  (await a.next('scan')).resolve({ files: FILES });
  await flush();
  ok(alert.hidden, 'a successful scan clears the alert');
  equal(fileIds(m).length, FILES.length);
  equal(localStorage.getItem(PATH_KEY), '"/tmp/tides"', 'a typed path is remembered');
});

test('alert: a busy mill is a warning with no report keys', async () => {
  const { a, m } = await sampled();
  pulpKey(m).click();
  (await a.next('pack')).reject(Object.assign(new Error('Another pack or preview is running.'), { busy: true }));
  await flush();
  const alert = alertBox(m);
  ok(!alert.hidden, 'the alert shows');
  match(text(alert), /busy/i);
  equal(alert.querySelector('[data-tone]').dataset.tone, 'warn');
  equal(alert.querySelector('[data-act="report-alert"]'), null, 'a busy answer is not reportable');
  equal(tone(m), 'idle', 'no dump came of it');
  ok(!unavailable(pulpKey(m)), 'Pulp can be pressed again');
  alert.querySelector('[data-act="dismiss"]').click();
  ok(alert.hidden, 'Dismiss hides the alert');
});

test('alert: Scan with no folder asks for one without scanning', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  m.$('.mill-source [data-act="scan"]').click();
  await flush();
  ok(!alertBox(m).hidden, 'the alert shows');
  match(text(alertBox(m)), /folder/i);
  equal(alertBox(m).querySelector('[data-tone]').dataset.tone, 'warn');
  equal(alertBox(m).querySelector('[data-act="report-alert"]'), null, 'nothing to report');
  equal(a.count('scan'), 0, 'no scan without a path');
  equal(document.activeElement, m.$('[data-el="path"]'), 'focus moves to the path field');
});

/* ---------- tree, filter, and focus ---------- */

test('preview: a slow preview never replaces a newer one', async () => {
  const { a, m } = await sampled();
  row(m, 'src/lib.rs').querySelector('[data-act="preview"]').click();
  const slow = await a.next('preview');
  row(m, 'src/main.rs').querySelector('[data-act="preview"]').click();
  const fast = await a.next('preview');
  fast.resolve({ text: 'fn main() {}\n', truncated: false });
  await flush();
  slow.resolve({ text: 'pub fn lib() {}\n', truncated: false });
  await flush();
  equal(slow.args[0].relative, 'src/lib.rs');
  equal(text(m.$('[data-el="prevbody"]')), 'fn main() {}', 'the newer preview stays');
  match(text(m.$('[data-el="prevhead"]')), /src\/main\.rs/);
  ok(row(m, 'src/main.rs').classList.contains('is-active'), 'the newer file is active');
  ok(!row(m, 'src/lib.rs').classList.contains('is-active'), 'the older file is not');
  equal(view(m), 'preview');
});

test('preview: a busy answer is retried after a pause, and the retry shows', async () => {
  const { a, m } = await sampled();
  row(m, 'src/lib.rs').querySelector('[data-act="preview"]').click();
  const first = await a.next('preview');
  const t0 = performance.now();
  first.reject(busyError());
  await flush();
  equal(a.count('preview'), 1, 'no retry at once');
  ok(m.$('[data-el="prevhead"] .p-spin'), 'the preview still shows it is loading');
  const retry = await a.next('preview');
  const waited = performance.now() - t0;
  ok(waited >= 100, 'the retry waits for the slot, but came after ' + Math.round(waited) + ' ms');
  equal(retry.args[0].relative, 'src/lib.rs', 'the retry asks for the same file');
  retry.resolve({ text: 'pub fn lib() {}\n', truncated: false });
  await flush();
  equal(text(m.$('[data-el="prevbody"]')), 'pub fn lib() {}', 'the retried preview shows');
  equal(m.$('[data-el="prevhead"] .p-spin'), null, 'and is no longer loading');
  ok(alertBox(m).hidden, 'a busy preview raises no alert');
});

test('preview: an older busy preview stops retrying once a newer one is clicked', async () => {
  const { a, m } = await sampled();
  row(m, 'src/lib.rs').querySelector('[data-act="preview"]').click();
  (await a.next('preview')).reject(busyError());
  await flush();
  // The older preview is now waiting out the busy slot.
  row(m, 'src/main.rs').querySelector('[data-act="preview"]').click();
  const newer = await a.next('preview');
  equal(newer.args[0].relative, 'src/main.rs');
  // The newer one waits too, and the older wait ends first: the next call
  // shows whether the older preview retried.
  newer.reject(busyError());
  const retry = await a.next('preview');
  equal(retry.args[0].relative, 'src/main.rs', 'only the newest preview retries');
  retry.resolve({ text: 'fn main() {}\n', truncated: false });
  await flush();
  equal(a.count('preview'), 3, 'the older preview gave up');
  equal(text(m.$('[data-el="prevbody"]')), 'fn main() {}', 'the newer preview shows');
  match(text(m.$('[data-el="prevhead"]')), /src\/main\.rs/);
});

test('filter: narrows the rows, and All or None only touch matches', async () => {
  const { m } = await sampled();
  typeFilter(m, '.rs');
  await waitFor(() => fileIds(m).length === RUST.length, { what: () => 'filtered rows, got ' + fileIds(m).join(', ') });
  same(fileIds(m).sort(), RUST.slice().sort());
  match(text(m.$('[data-el="footsel"]')), /\b3 match/, 'the footer counts the matches');
  m.$('[data-act="none"]').click();
  ok(RUST.every((id) => !ticked(m, id)), 'None unticks the matches');
  same(counts(m), [TICKED.length - RUST.length, FILES.length], 'files outside the filter keep their ticks');

  typeFilter(m, 'zzz');
  const none = await waitFor(() => visible(m.$('[data-el="tree"] .mill-none')) && m.$('[data-el="tree"] .mill-none'), { what: 'the no-match note' });
  match(text(none), /zzz/, 'the note quotes the filter');
  equal(fileIds(m).length, 0, 'no rows match');
  typeFilter(m, '');
  await waitFor(() => fileIds(m).length === FILES.length, { what: 'every row back' });
  ok(!visible(m.$('[data-el="tree"] .mill-none')), 'the no-match note goes');
  m.$('[data-act="all"]').click();
  same(counts(m), [FILES.length - 1, FILES.length], 'All ticks all but the oversized file');
});

test('folders: a folder box ticks everything under it and shows a mixed state', async () => {
  const { m } = await sampled();
  const dirBox = (path) => m.$('.mill-row[data-dir="' + path + '"] [data-act="dircheck"]');
  ok(dirBox('src').checked && !dirBox('src').indeterminate, 'src starts fully ticked');
  dirBox('src').click();
  ok(RUST.every((id) => !ticked(m, id)), 'unticking src unticks its whole subtree');
  ok(!dirBox('src').checked && !dirBox('src').indeterminate, 'src is empty');
  toggle(m, 'src/net/mod.rs');
  ok(dirBox('src').indeterminate, 'one ticked file makes src mixed');
  ok(dirBox('src/net').checked, 'src/net is fully ticked');

  ok(dirBox('data').indeterminate, 'data starts mixed');
  dirBox('data').click();
  ok(ticked(m, 'data/archive.zip') && ticked(m, 'data/readings.csv'), 'ticking data ticks its files');
  ok(!ticked(m, 'data/big.csv'), 'but never an oversized one');
  ok(dirBox('data').checked && !dirBox('data').indeterminate, 'data is full');
});

test('tree: a large tree draws a window of rows and keeps one tab stop', async () => {
  const { m, total } = await expanded(5, 400);
  equal(m.api.state.collapsed.size, 0, 'every folder is open');
  ok(drawn(m) > 0 && drawn(m) < total / 10, 'a window of the ' + total + ' rows is drawn, got ' + drawn(m));
  let stops = tabStops(m);
  ok(stops.length === 1 && stops[0].closest('.mill-row[data-dir="d0"]'), 'one tab stop, on the first row: ' + stops.map(show).join(', '));

  // Scroll the focused row out of the drawn window.
  stops[0].focus();
  const tree = m.$('[data-el="tree"]');
  tree.scrollTop = tree.scrollHeight / 2;
  await waitFor(() => !dirRow(m, 'd0'), { what: 'the drawn window to move off the first row' });
  ok(drawn(m) < total / 10, 'still only a window is drawn, got ' + drawn(m));
  const rows = m.$('[data-el="rows"]');
  stops = tabStops(m);
  ok(stops.length === 1 && stops[0] === rows, 'the tree itself holds the one tab stop: ' + stops.map(show).join(', '));
  equal(document.activeElement, rows, 'focus waits on the tree, got ' + show(document.activeElement));
  press(rows, 'ArrowDown');
  const back = focusedRow();
  equal(back && back.dataset.id, 'd0/f000.txt', 'an arrow key brings focus back to the rows, got ' + show(document.activeElement));
  equal(tabStops(m).length, 1, 'one tab stop again');
});

test('tree: End, Home, and the arrows carry focus and the one tab stop through a large tree', async () => {
  const { m, total } = await expanded(5, 400);
  tabStops(m)[0].focus();
  press(document.activeElement, 'End');
  const last = focusedRow();
  ok(last, 'End leaves focus on a row, got ' + show(document.activeElement));
  equal(last.dataset.id, 'd4/f399.txt', 'End focuses the last row');
  same([last.getAttribute('aria-posinset'), last.getAttribute('aria-setsize')], ['400', '400'], 'the last of its folder\'s files');
  ok(drawn(m) < total / 10, 'only a window is drawn at the end, got ' + drawn(m));
  let stops = tabStops(m);
  ok(stops.length === 1 && stops[0] === document.activeElement, 'the focused row holds the one tab stop: ' + stops.map(show).join(', '));

  press(document.activeElement, 'Home');
  const first = focusedRow();
  ok(first, 'Home leaves focus on a row, got ' + show(document.activeElement));
  equal(first.dataset.dir, 'd0', 'Home focuses the first row');
  same([first.getAttribute('aria-posinset'), first.getAttribute('aria-setsize')], ['1', '5'], 'the first of the top-level folders');
  stops = tabStops(m);
  ok(stops.length === 1 && stops[0] === document.activeElement, 'the focused row holds the one tab stop: ' + stops.map(show).join(', '));

  press(document.activeElement, 'ArrowDown');
  const next = focusedRow();
  equal(next && next.dataset.id, 'd0/f000.txt', 'ArrowDown moves to the next row, got ' + show(document.activeElement));
  stops = tabStops(m);
  ok(stops.length === 1 && stops[0] === document.activeElement, 'the tab stop moves with focus: ' + stops.map(show).join(', '));
});

test('focus: activating a folder twist keeps focus on that twist', async () => {
  const { m } = await sampled();
  const twist = m.$('.mill-row[data-dir="src"] .mill-twist');
  twist.focus();
  equal(document.activeElement, twist, 'the twist takes focus');
  // Enter or Space on a focused button clicks it.
  twist.click();
  const after = document.activeElement;
  ok(after && after.matches('.mill-row[data-dir="src"] .mill-twist'), 'focus stays on the src twist, got ' + show(after));
  equal(after.getAttribute('aria-expanded'), 'false', 'src is collapsed');
  equal(row(m, 'src/lib.rs'), null, 'its files are hidden');
  after.click();
  ok(document.activeElement.matches('.mill-row[data-dir="src"] .mill-twist'), 'focus stays after expanding, got ' + show(document.activeElement));
  ok(row(m, 'src/lib.rs'), 'its files are back');
});

test('focus: Enter on a folder row toggles it and keeps focus on its checkbox', async () => {
  const { m } = await sampled();
  const check = m.$('.mill-row[data-dir="src"] [data-act="dircheck"]');
  check.focus();
  press(check, 'Enter');
  const after = document.activeElement;
  ok(after && after.matches('.mill-row[data-dir="src"] [data-act="dircheck"]'), 'focus stays on the src checkbox, got ' + show(after));
  equal(row(m, 'src/lib.rs'), null, 'Enter collapsed src');
  ok(after.checked, 'Enter leaves the tick alone');
});

test('focus: activating a language chip keeps focus on that chip', async () => {
  const { m } = await sampled();
  const chip = m.$('[data-act="chip"][data-lang="rust"]');
  chip.focus();
  chip.click();
  const after = document.activeElement;
  ok(after && after.matches('[data-act="chip"][data-lang="rust"]'), 'focus stays on the rust chip, got ' + show(after));
  equal(after.dataset.state, 'off');
  ok(RUST.every((id) => !ticked(m, id)), 'every rust file is unticked');
  after.click();
  ok(document.activeElement.matches('[data-act="chip"][data-lang="rust"]'), 'focus stays after ticking, got ' + show(document.activeElement));
  equal(document.activeElement.dataset.state, 'on');
});

test('focus: Try a sample moves focus into the tree once the files are listed', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  const key = m.$('.mill-empty [data-act="sample"]');
  key.focus();
  key.click();
  // The key hides while the sample loads; a real request spans frames, so let
  // the page render, which drops focus from a hidden control.
  const sample = await a.next('sample');
  await frames();
  sample.resolve({ path: '/tmp/tides', sample: true });
  const scan = await a.next('scan');
  await frames();
  scan.resolve({ files: FILES });
  await flush();
  ok(m.$('[data-el="rows"]').contains(document.activeElement), 'focus is in the tree, got ' + show(document.activeElement));
});

test('focus: Browse into a folder with nothing to pulp keeps focus in the mill', async () => {
  const a = fakeAdapter();
  const m = mount(a);
  const key = m.$('.mill-empty [data-act="browse"]');
  key.focus();
  key.click();
  (await a.next('browse')).resolve({ path: '/tmp/empty' });
  const scan = await a.next('scan');
  await frames();
  scan.resolve({ files: [] });
  await flush();
  ok(m.$('.mill-nofiles'), 'the folder has nothing to pulp');
  // There is no tree to take focus, and with nothing ticked the Pulp key is
  // disabled; focus has to land on something that can hold it.
  ok(m.root.contains(document.activeElement), 'focus stays in the mill, got ' + show(document.activeElement));
});

test('focus: Pulp again hands focus to the Pulp key while the pack runs', async () => {
  const { a, m } = await pulped();
  toggle(m, 'README.md');
  const again = m.$('.mill-outnote [data-act="pulp"]');
  again.focus();
  again.click();
  const pack = await a.next('pack');
  equal(document.activeElement, pulpKey(m), 'the note goes and focus moves to the Pulp key, got ' + show(document.activeElement));
  await frames();
  equal(document.activeElement, pulpKey(m), 'focus stays on the Pulp key while the pack runs, got ' + show(document.activeElement));
  pack.resolve(packResult());
  await flush();
  equal(status(m), 'Ready');
  equal(document.activeElement, pulpKey(m), 'and after it, got ' + show(document.activeElement));
});

/* ---------- layout and settings ---------- */

test('phone: tabs and a dock that follow the state', async () => {
  const a = fakeAdapter();
  const m = mount(a, { width: 390 });
  const dock = m.$('[data-el="dock"]');
  ok(visible(m.$('[data-el="tabs"]')), 'the Files and Output tabs show');
  ok(visible(dock), 'the action dock shows');
  ok(!visible(pulpKey(m)), 'the side Pulp key gives way to the dock');
  same(acts(dock), ['browse', 'sample']);
  equal(m.root.dataset.tab, 'files');
  ok(visible(m.$('.mill-files')) && !visible(m.$('.mill-out')), 'Files shows first');

  dock.querySelector('[data-act="sample"]').click();
  (await a.next('sample')).resolve({ path: '/tmp/tides', sample: true });
  (await a.next('scan')).resolve({ files: FILES });
  await flush();
  same(acts(dock), ['pulp']);
  match(text(dock), new RegExp('\\b' + TICKED.length + ' files\\b'), 'the dock Pulp key counts the ticks');

  dock.querySelector('[data-act="pulp"]').click();
  const pack = await a.next('pack');
  same(acts(dock), ['cancel'], 'the dock offers Cancel while packing');
  pack.resolve(packResult());
  await flush();
  same(acts(dock), ['copy', 'download', 'tree'], 'the dock offers the Ready keys');
  equal(m.root.dataset.tab, 'output');
  ok(visible(m.$('.mill-out')) && !visible(m.$('.mill-files')), 'the output shows after a pulp');

  const filesTab = m.$('[data-tab="files"]');
  filesTab.click();
  equal(m.root.dataset.tab, 'files');
  equal(filesTab.getAttribute('aria-selected'), 'true');
  ok(visible(m.$('.mill-files')) && !visible(m.$('.mill-out')), 'the Files tab shows the tree');
  press(filesTab, 'ArrowRight');
  equal(m.root.dataset.tab, 'output', 'arrow keys switch tabs');
  equal(document.activeElement, m.$('[data-tab="output"]'), 'focus follows the tab');
  ok(!visible(m.$('.mill-set')), 'settings fold behind a key');
  m.$('[data-act="insp"]').click();
  ok(visible(m.$('.mill-set')), 'Settings opens them');
});

test('phone: a dock tap just after the keys swap is ignored', async () => {
  const { a, m } = await sampled(undefined, 390);
  const dock = m.$('[data-el="dock"]');
  dock.querySelector('[data-act="pulp"]').click();
  const swapped = performance.now();
  const pack = await a.next('pack');
  same(acts(dock), ['cancel'], 'Cancel takes the place of Pulp');
  dock.querySelector('[data-act="cancel"]').click();
  await flush();
  equal(a.count('cancel'), 0, 'a tap within 400 ms of the swap is ignored');
  // The guard holds for 400 ms from the swap, which happened inside the click.
  await sleep(swapped + 450 - performance.now());
  dock.querySelector('[data-act="cancel"]').click();
  await flush();
  equal(a.count('cancel'), 1, 'a later tap cancels');

  pack.resolve({ cancelled: true });
  await flush();
  same(acts(dock), ['pulp'], 'Pulp comes back');
  dock.querySelector('[data-act="pulp"]').click();
  await flush();
  equal(a.count('pack'), 1, 'a tap just after the swap back is ignored too');
});

test('settings: choices persist to the next mount, and a demo leaves storage alone', async () => {
  const first = mount(fakeAdapter());
  first.$('input[data-setting="format"][value="md"]').click();
  first.$('[data-opt="hidden"]').click();
  const stored = JSON.parse(localStorage.getItem(SETTINGS_KEY));
  equal(stored.format, 'md');
  equal(stored.hidden, true);

  const second = mount(fakeAdapter(), { keepStorage: true });
  ok(second.$('input[data-setting="format"][value="md"]').checked, 'the format comes back');
  ok(second.$('[data-opt="hidden"]').checked, 'the option comes back');

  const demo = mount(fakeAdapter(), { keepStorage: true, options: { demo: true, path: '~/Projects/tides' } });
  ok(demo.$('input[data-setting="format"][value="xml"]').checked, 'a demo starts from the defaults');
  demo.$('input[data-setting="format"][value="txt"]').click();
  equal(JSON.parse(localStorage.getItem(SETTINGS_KEY)).format, 'md', 'a demo does not write settings');
  equal(demo.$('[data-el="path"]').value, '~/Projects/tides');
  equal(demo.$('header, main, footer, h1'), null, 'a demo adds no landmarks or headings to its page');
});

test('keys: Ctrl+Enter pulps from anywhere on the page', async () => {
  const { a, m } = await sampled();
  press(document.body, 'Enter', { ctrlKey: true });
  const pack = await a.next('pack');
  same(pack.args[0].selected.slice().sort(), TICKED.slice().sort());
  pack.resolve(packResult());
  await flush();
  equal(status(m), 'Ready');
});

test('destroy: Esc and Ctrl+Enter reach neither a replaced mill nor a destroyed one', async () => {
  const earlier = await sampled();
  const { a, m } = await sampled();
  pulpKey(m).click();
  const pack = await a.next('pack');
  m.api.destroy();
  press(document.body, 'Escape');
  await flush();
  equal(a.count('cancel'), 0, 'Esc no longer cancels');
  pack.resolve(packResult());
  await flush();
  equal(status(m), 'Ready');
  press(document.body, 'Enter', { ctrlKey: true });
  await flush();
  equal(a.count('pack'), 1, 'Ctrl+Enter no longer pulps');
  equal(earlier.a.count('pack'), 0, 'nor does it reach the mill the second mount replaced');
});

/* ---------- units ---------- */

test('human: sizes step up through B, KiB, MiB, and GiB', async () => {
  equal(human(0), '0 B');
  equal(human(1023), '1023 B');
  equal(human(1536), '1.5 KiB');
  equal(human(5 * 1024 * 1024), '5.0 MiB');
  equal(human(3 * 1024 ** 3), '3.0 GiB');
});

run();
