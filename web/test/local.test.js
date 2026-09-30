// End-to-end tests of the local mill. `cargo xtask ui-test` builds pulp, starts
// a real `pulp ui`, and serves this page from an origin that forwards every
// path outside /web/ and /__ui-test/ to it. Each test opens the shell in a
// window of its own, from the link pulp printed, and uses it as a person
// would, so web/index.html's adapter runs against the real server and every
// header pulp sends: the session token header, the busy and stale-session
// answers, progress polling, the sample's display name, and the full-dump
// fetch.

import { equal, match, ok, run, same, show, sleep, test, waitFor } from './harness.js';

const SETTINGS_KEY = 'pulp.mill.settings';
const PATH_KEY = 'pulp.mill.path';
const TESTDATA = ['Hello.lean', 'hello.rs'];

/**
 * What the harness prepared: `page`, the path of the link pulp printed (it
 * carries the session token, and changes when pulp restarts); the folders
 * `testdata` (the repository's), `big` (a dump over the mill's preview cap,
 * whose last line is `bigEnd`), and `slow` (`slowFiles` heavy files that pulp
 * extracts one at a time).
 */
async function harnessEnv() {
  const res = await fetch('/__ui-test/env');
  if (!res.ok) {
    throw new Error('no /__ui-test/env here (' + res.status + '): open this page from the mill origin that `cargo xtask ui-test --serve` prints');
  }
  return res.json();
}

const env = await harnessEnv();

/* ---------- mill windows ---------- */

/** Windows the tests opened; each test starts by closing the last test's. */
const windows = [];

/** The window's document once `win` has loaded a page other than `previous`. */
async function loaded(win, previous) {
  const doc = await waitFor(() => {
    // A window whose opener policy differs from this page's is cut off at once.
    if (win.closed) return 'closed';
    try {
      const doc = win.document;
      return doc !== previous && doc.readyState === 'complete' && win.location.pathname === '/' && doc;
    } catch (_) {
      // Between documents while it navigates.
      return false;
    }
  }, { timeout: 20000, what: () => 'the mill window to load' });
  ok(doc !== 'closed', 'the mill window is gone: it must send the opener policy this page does');
  return doc;
}

/**
 * Open the mill from the session link in a new window, beside the windows
 * already open when `keep` is set, and return a handle to drive and read it.
 */
async function openMill({ keep = false } = {}) {
  if (!keep) {
    for (const win of windows.splice(0)) win.close();
    await idle();
    // The mill keeps settings per origin; start every test from the defaults.
    localStorage.removeItem(SETTINGS_KEY);
    localStorage.removeItem(PATH_KEY);
  }
  const { page } = await harnessEnv();
  const win = window.open(page, '', 'width=1280,height=900');
  ok(win, 'the browser opened a window for the mill');
  windows.push(win);
  await loaded(win, null);
  return instrument(win);
}

/**
 * Leave pulp ui idle for the next test, whatever the last one left behind:
 * held progress polls go through, and a pack still running is cancelled.
 */
async function idle() {
  await fetch('/__ui-test/release-polls', { method: 'POST' });
  const { page } = await harnessEnv();
  const token = new URLSearchParams(page.split('?')[1] || '').get('token');
  if (!token) return;
  const headers = { 'content-type': 'application/json', 'x-pulp-token': token };
  for (let tries = 0; tries < 100; tries++) {
    const res = await fetch('/api/progress', { headers });
    if (!res.ok || !(await res.json()).running) return;
    await fetch('/api/cancel', { method: 'POST', headers, body: '{}' });
    await sleep(100);
  }
  throw new Error('pulp ui kept packing through 10 s of cancels');
}

/**
 * Stub the window's clipboard, record every fetch it makes (with its session
 * token, status, and a progress poll's answer), and collect uncaught errors.
 */
function instrument(win) {
  const doc = win.document;
  const m = {
    win,
    doc,
    root: doc.getElementById('mill'),
    token: (doc.querySelector('meta[name="pulp-token"]') || {}).content || '',
    requests: [],
    clipboard: [],
    errors: [],
    $: (selector) => doc.querySelector(selector),
    $$: (selector) => [...doc.querySelectorAll(selector)],
  };
  ok(m.root, 'pulp ui served the mill at ' + win.location.pathname + win.location.search.replace(/token=[^&]+/, 'token=…') +
    ', not "' + doc.title + '": ' + (doc.body ? doc.body.textContent.trim().slice(0, 200) : ''));
  const realFetch = win.fetch.bind(win);
  win.fetch = async (input, init) => {
    const url = new URL(String(input), win.location.href);
    const headers = new win.Headers((init && init.headers) || {});
    const call = {
      method: (init && init.method) || 'GET',
      path: url.pathname,
      query: url.search,
      token: headers.get('x-pulp-token') || '',
      status: 0,
      answer: null,
    };
    m.requests.push(call);
    try {
      const res = await realFetch(input, init);
      call.status = res.status;
      if (call.path === '/api/progress' && res.ok) call.answer = await res.clone().json();
      return res;
    } catch (err) {
      call.status = -1;
      throw err;
    }
  };
  Object.defineProperty(win.navigator, 'clipboard', {
    configurable: true,
    value: {
      async writeText(text) {
        m.clipboard.push(String(text));
      },
      async write(items) {
        for (const item of items) m.clipboard.push(await (await item.getType('text/plain')).text());
      },
    },
  });
  win.addEventListener('error', (e) => m.errors.push(e.error || new Error(e.message)));
  win.addEventListener('unhandledrejection', (e) => m.errors.push(e.reason));
  return m;
}

/* ---------- reading the shell ---------- */

const text = (el) => (el ? el.textContent.trim() : '');
const status = (m) => text(m.$('[data-el="status"]'));
const tone = (m) => m.$('[data-el="status"]').dataset.tone;
const fileIds = (m) => m.$$('.mill-row[data-id]').map((row) => row.dataset.id);
/** The files pane's ticked and total counts. */
const counts = (m) => (text(m.$('[data-el="filecount"]')).match(/\d+/g) || []).map(Number);
/** The dump as the sheet shows it, one line per ledger line. */
const dumpText = (m) => m.$$('[data-el="dump"] .ln:not([aria-hidden="true"])').map((line) => line.textContent).join('');
const shownAlert = (m) => {
  const alert = m.$('[data-el="alert"]');
  return alert && !alert.hidden ? alert : null;
};
const lastToast = (m) => text(m.$$('[data-el="toasts"] > *').pop());
/** Recorded calls to `path`, optionally only those with `method`. */
const calls = (m, path, method) => m.requests.filter((call) => call.path === path && (!method || call.method === method));

/** A one-line account of the shell, for failure messages. */
function describe(m) {
  const alert = shownAlert(m);
  return 'status "' + status(m) + '", busy "' + (m.root.dataset.busy || '') + '", files ' + show(counts(m)) +
    (alert ? ', alert "' + text(alert) + '"' : '') +
    ', calls ' + m.requests.map((call) => call.method + ' ' + call.path + ' ' + call.status).join('; ');
}

/** No uncaught errors in the shell, and every call it made carried its token. */
function clean(m) {
  equal(m.errors.length, 0, 'uncaught errors in the shell: ' + m.errors.map((err) => String(err && err.message || err)).join('; '));
  const bare = m.requests.filter((call) => call.path.startsWith('/api/') && call.token !== m.token);
  equal(bare.length, 0, 'calls without the session token: ' + show(bare));
}

/* ---------- using the shell ---------- */

/** Type `path` into the folder field and press Enter; wait for the scan. */
async function scan(m, path) {
  const field = m.$('[data-el="path"]');
  field.value = path;
  field.dispatchEvent(new m.win.Event('input', { bubbles: true }));
  const before = calls(m, '/api/scan').length;
  field.dispatchEvent(new m.win.KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }));
  await waitFor(() => calls(m, '/api/scan').length > before && !m.root.dataset.busy, {
    timeout: 20000,
    what: () => 'the scan of ' + path + ': ' + describe(m),
  });
  ok(!shownAlert(m), 'the scan of ' + path + ' succeeds: ' + describe(m));
}

/** Press Pulp and wait for the dump. */
async function pulp(m) {
  m.$('[data-el="pulpkey"]').click();
  await waitFor(() => !m.root.dataset.busy && (shownAlert(m) || tone(m) === 'ok' || tone(m) === 'warn'), {
    timeout: 30000,
    what: () => 'the pack: ' + describe(m),
  });
  ok(!shownAlert(m), 'the pack succeeds: ' + describe(m));
}

/* ---------- tests ---------- */

test('shell: pulp ui serves the mill with its session token and version filled in', async () => {
  const m = await openMill();
  match(m.token, /^[0-9a-f]{32}$/, 'a 128-bit session token');
  const linked = new URLSearchParams(env.page.split('?')[1] || '').get('token');
  ok(linked === null || linked === m.token, 'the page holds the token of the link it was opened from');
  ok(!m.doc.documentElement.innerHTML.includes('__PULP_'), 'no placeholder is left in the page');
  equal(m.root.dataset.surface, 'local');
  equal(status(m), 'Not generated');
  match(text(m.$('.mill-foot')), /pulp \d+\.\d+\.\d+/, 'the footer names the version');
  clean(m);
}, { timeout: 20000 });

test('scan: a typed folder lists testdata/ from the real walk', async () => {
  const m = await openMill();
  await scan(m, env.testdata);
  same(fileIds(m).sort(), TESTDATA, 'one row per file in testdata/');
  same(counts(m), [2, 2], 'both files are ticked');
  const scans = calls(m, '/api/scan', 'POST');
  equal(scans.length, 1, 'one scan request');
  equal(scans[0].status, 200);
  equal(localStorage.getItem(PATH_KEY), JSON.stringify(env.testdata), 'a typed folder is remembered');
  clean(m);
}, { timeout: 30000 });

test('pack and redraw: Pulp makes the XML dump, and Markdown redraws it without a new pack', async () => {
  const m = await openMill();
  await scan(m, env.testdata);
  await pulp(m);
  equal(status(m), 'Ready');
  const xml = dumpText(m);
  match(xml, /^<documents>/, 'the dump is XML by default');
  ok(xml.includes('pub fn hello()') && xml.includes('def hello'), 'both files are in the dump');
  match(text(m.$('[data-el="statmeta"]')), /tokens/, 'the token estimate shows');

  m.$('input[data-setting="format"][value="md"]').click();
  await waitFor(() => calls(m, '/api/render').length && !m.root.dataset.busy && dumpText(m) !== xml, {
    timeout: 20000,
    what: () => 'the redraw: ' + describe(m),
  });
  equal(status(m), 'Ready');
  const md = dumpText(m);
  match(md, /^## hello\.rs$/m, 'the redraw is Markdown');
  ok(md.includes('```rust') && md.includes('pub fn hello()'), 'with the source fenced');
  equal(calls(m, '/api/render', 'POST').length, 1, 'one redraw');
  equal(calls(m, '/api/render', 'POST')[0].status, 200);
  equal(calls(m, '/api/pack', 'POST').length, 1, 'no new pack for a redraw');
  clean(m);
}, { timeout: 60000 });

test('preview: a file row shows the file as pulp extracts it', async () => {
  const m = await openMill();
  await scan(m, env.testdata);
  m.$('.mill-row[data-id="hello.rs"] [data-act="preview"]').click();
  await waitFor(() => text(m.$('[data-el="prevbody"]')).includes('pub fn hello()'), {
    timeout: 20000,
    what: () => 'the preview of hello.rs, got "' + text(m.$('[data-el="prevbody"]')) + '": ' + describe(m),
  });
  match(text(m.$('[data-el="prevhead"]')), /hello\.rs/);
  equal(calls(m, '/api/preview', 'POST').at(-1).status, 200);
  clean(m);
}, { timeout: 40000 });

test('copy and tree: Copy puts the dump on the clipboard, and Tree the directory map', async () => {
  const m = await openMill();
  await scan(m, env.testdata);
  await pulp(m);
  m.$('.mill-outkeys [data-act="copy"]').click();
  await waitFor(() => m.clipboard.length === 1, { what: () => 'the Copy write: ' + describe(m) });
  equal(m.clipboard[0].trimEnd(), dumpText(m).trimEnd(), 'the clipboard holds the dump shown');
  match(lastToast(m), /pulp\.xml/, 'the Copy toast names the dump');

  m.$('.mill-outkeys [data-act="tree"]').click();
  await waitFor(() => m.clipboard.length === 2, { timeout: 20000, what: () => 'the Tree write: ' + describe(m) });
  ok(m.clipboard[1].includes('hello.rs') && m.clipboard[1].includes('Hello.lean'), 'the map lists both files: ' + show(m.clipboard[1]));
  ok(!m.clipboard[1].includes('pub fn hello'), 'the map has no file contents');
  equal(calls(m, '/api/tree', 'POST').at(-1).status, 200);
  equal(m.requests.filter((call) => call.path.startsWith('/api/artifact/')).length, 0, 'a whole dump needs no second fetch');
  clean(m);
}, { timeout: 60000 });

test('full dump: Copy of a dump over the preview cap fetches the whole dump from pulp ui', async () => {
  const m = await openMill();
  await scan(m, env.big);
  await pulp(m);
  ok(!m.$('[data-el="outnote"]').hidden, 'the clipped-dump note shows: ' + describe(m));
  const shown = dumpText(m);
  ok(!shown.includes(env.bigEnd), 'the end of the folder is past the preview');

  m.$('.mill-outkeys [data-act="copy"]').click();
  await waitFor(() => m.clipboard.length === 1, { timeout: 20000, what: () => 'the Copy write: ' + describe(m) });
  const whole = m.clipboard[0];
  ok(whole.includes(env.bigEnd), 'the clipboard holds the whole dump, ' + whole.length + ' chars');
  ok(whole.length > shown.length, 'longer than the preview');
  ok(whole.startsWith(shown.slice(0, 1000)), 'and starts as the preview does');
  const fetched = m.requests.filter((call) => call.path.startsWith('/api/artifact/'));
  equal(fetched.length, 1, 'one full-dump fetch');
  equal(fetched[0].status, 200);
  match(fetched[0].query, /format=xml/, 'in the dump format');
  clean(m);
}, { timeout: 60000 });

test('progress, busy, cancel: a slow pack reports files read, a second tab hears the mill is busy, and Cancel stops it', async () => {
  const other = await openMill();
  await scan(other, env.testdata);
  const m = await openMill({ keep: true });
  await scan(m, env.slow);
  same(counts(m), [env.slowFiles, env.slowFiles], 'every heavy file is ticked');

  m.$('[data-el="pulpkey"]').click();
  // Only an answer from /api/progress moves the count past zero.
  await waitFor(() => /^[1-9]\d* of \d+ files$/.test(text(m.$('[data-el="why"]'))), {
    timeout: 20000,
    what: () => 'files read to show, got "' + text(m.$('[data-el="why"]')) + '": ' + describe(m),
  });
  match(text(m.$('[data-el="why"]')), new RegExp(' of ' + env.slowFiles + ' files$'), 'progress counts every ticked file');
  ok('p' in m.$('[data-el="progress"]').dataset, 'the bar shows measured progress');
  const running = calls(m, '/api/progress', 'GET').filter((call) => call.status === 200 && call.answer && call.answer.running);
  ok(running.length > 0, 'a poll found the pack running');
  equal(running[0].answer.total, env.slowFiles);

  other.$('[data-el="pulpkey"]').click();
  await waitFor(() => shownAlert(other), { timeout: 20000, what: () => 'the second tab\'s busy answer: ' + describe(other) });
  match(text(shownAlert(other)), /busy/i);
  match(text(shownAlert(other)), /another pack is running/i);
  equal(shownAlert(other).querySelector('[data-tone]').dataset.tone, 'warn');
  equal(shownAlert(other).querySelector('[data-act="report-alert"]'), null, 'a busy mill is not reportable');
  equal(calls(other, '/api/pack', 'POST').at(-1).status, 429);
  equal(tone(other), 'idle', 'the second tab has no dump');

  equal(m.root.dataset.busy, 'pack', 'the slow pack is still running: ' + describe(m));
  // End the pack while a progress poll is out: the harness holds the next one.
  const hold = await (await fetch('/__ui-test/hold-polls', { method: 'POST' })).json();
  ok(hold.held, 'a progress poll is held');
  m.$('[data-el="cancelkey"]').click();
  await waitFor(() => !m.root.dataset.busy, { timeout: 20000, what: () => 'the pack to stop: ' + describe(m) });
  equal(calls(m, '/api/cancel', 'POST').at(-1).status, 204);
  match(lastToast(m), /cancelled/i, 'the mill says the pack was cancelled');
  equal(status(m), 'Not generated', 'a cancelled pack leaves no dump');
  ok(!shownAlert(m), 'a cancel is not an error: ' + describe(m));
  await fetch('/__ui-test/release-polls', { method: 'POST' });
  await waitFor(() => calls(m, '/api/progress').every((call) => call.status), { what: 'the held poll to answer' });
  const polls = calls(m, '/api/progress').length;
  await sleep(1000);
  equal(calls(m, '/api/progress').length, polls, 'progress polling stops with the pack, even with a poll out when it ended');
  clean(m);
  clean(other);
}, { timeout: 90000 });

test('sample: Try a sample scans it under its name and pulps its PDF to text', async () => {
  const m = await openMill();
  m.$('.mill-empty [data-act="sample"]').click();
  await waitFor(() => calls(m, '/api/scan').length && !m.root.dataset.busy, {
    timeout: 20000,
    what: () => 'the sample scan: ' + describe(m),
  });
  ok(!shownAlert(m), 'the sample scans: ' + describe(m));
  equal(calls(m, '/api/sample', 'POST')[0].status, 200);
  equal(m.$('[data-el="path"]').value, 'tides (sample)', 'the field names the sample, not its temp folder');
  equal(localStorage.getItem(PATH_KEY), null, 'the temp folder is not remembered');
  ok(counts(m)[1] >= 10, 'the sample files are listed: ' + describe(m));

  await pulp(m);
  const dump = dumpText(m);
  ok(/storm surge/.test(dump) && !/endobj/.test(dump), 'pulp ui extracted the sample PDF to text');
  clean(m);
}, { timeout: 60000 });

test('stale session: after pulp ui restarts, the open page says so, its old link is locked, and the new link works', async () => {
  const m = await openMill();
  await scan(m, env.testdata);
  const before = await harnessEnv();
  const restarted = await fetch('/__ui-test/restart', { method: 'POST' });
  equal(restarted.status, 200, 'the harness restarted pulp ui: ' + (await restarted.clone().text()));

  m.$('.mill-source [data-act="scan"]').click();
  await waitFor(() => shownAlert(m), { timeout: 20000, what: () => 'the stale-session answer: ' + describe(m) });
  const alert = shownAlert(m);
  match(text(alert), /restarted/i, 'the alert says the mill restarted');
  equal(alert.querySelector('[data-tone]').dataset.tone, 'warn');
  equal(alert.querySelector('[data-act="report-alert"]'), null, 'a stale session is not reportable');
  equal(calls(m, '/api/scan', 'POST').at(-1).status, 401);

  const after = await harnessEnv();
  const doc = m.doc;
  m.win.location.reload();
  const reloaded = await loaded(m.win, doc);
  if (after.page === before.page) {
    // A link without a token: reloading it reaches the new session.
    ok(reloaded.getElementById('mill'), 'a reload shows the mill again');
  } else {
    equal(reloaded.getElementById('mill'), null, 'the old link no longer opens the mill');
    ok(!reloaded.documentElement.innerHTML.includes(m.token), 'the locked page does not hold the old token');
  }

  const again = await openMill({ keep: true });
  match(again.token, /^[0-9a-f]{32}$/);
  ok(again.token !== m.token, 'the new session has a new token');
  await scan(again, env.testdata);
  same(fileIds(again).sort(), TESTDATA, 'the new link scans again');
  clean(again);
}, { timeout: 60000 });

run();
