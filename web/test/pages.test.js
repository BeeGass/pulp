// Page tests: load the site as it deploys (the landing page at / and the
// browser mill at /mill/, both served from site/ by `cargo xtask ui-test`) in
// same-origin frames and use them the way a visitor would.

import { equal, match, ok, run, same, sleep, test, waitFor } from './harness.js';

const stage = document.getElementById('stage');
// Lines of a dump; the ruling on an empty sheet is aria-hidden.
const DUMP_LINE_SELECTOR = '[data-el="dump"] .ln:not([aria-hidden="true"])';

/** Disabled, or marked aria-disabled while it stays focusable. */
const unavailable = (el) => el.disabled || el.getAttribute('aria-disabled') === 'true';

/** Load `path` in a fresh frame and return its document once it has loaded. */
async function open(path) {
  // The mill keeps settings per origin; start every page from the defaults.
  localStorage.removeItem('pulp.mill.settings');
  localStorage.removeItem('pulp.mill.path');
  const frame = document.createElement('iframe');
  frame.title = path;
  const loaded = new Promise((resolve) => frame.addEventListener('load', resolve, { once: true }));
  frame.src = path;
  stage.replaceChildren(frame);
  await loaded;
  return frame.contentDocument;
}

/** A one-line account of a mounted mill, for failure messages. */
function describe(root) {
  if (!root) return 'no mill on the page';
  const find = (selector) => root.querySelector(selector);
  const status = find('[data-el="status"]');
  const alert = find('[data-el="alert"]');
  return 'status "' + (status ? status.textContent.trim() : '-') + '", busy "' + (root.dataset.busy || '') + '", ' +
    root.querySelectorAll('.mill-row[data-id]').length + ' file rows' +
    (alert && !alert.hidden ? ', alert "' + alert.textContent.trim() + '"' : '');
}

test('landing page: the product shot reaches Ready', async () => {
  const doc = await open('/');
  const shot = doc.getElementById('shot');
  ok(shot, 'the page has the product shot');
  await waitFor(() => {
    const status = shot.querySelector('[data-el="status"]');
    return status && status.textContent === 'Ready';
  }, { timeout: 15000, what: () => 'the shot to pulp its demo: ' + describe(shot) });
  ok(!doc.querySelector('.lp-shot').hidden, 'the shot is shown');
  ok(shot.querySelectorAll('.mill-row[data-id]').length >= 10, 'the demo files are listed');
  ok(shot.querySelectorAll(DUMP_LINE_SELECTOR).length >= 10, 'the demo dump is drawn');
}, { timeout: 20000 });

test('browser mill: Try a sample, then Pulp, reaches Ready', async () => {
  const doc = await open('/mill/');
  const mill = doc.getElementById('mill');
  const alerted = () => !doc.querySelector('[data-el="alert"]').hidden;
  const sample = await waitFor(() => {
    const key = doc.querySelector('.mill-empty [data-act="sample"]');
    return key && !unavailable(key) && key;
  }, { timeout: 10000, what: () => 'Try a sample: ' + describe(mill) });

  sample.click();
  await waitFor(() => !mill.dataset.busy && (alerted() || mill.querySelector('.mill-row[data-id]')), {
    timeout: 20000,
    what: () => 'the sample scan: ' + describe(mill),
  });
  ok(!alerted(), 'the sample scans cleanly: ' + describe(mill));
  const pulp = doc.querySelector('[data-el="pulpkey"]');
  ok(!unavailable(pulp), 'Pulp is available after the scan: ' + describe(mill));

  pulp.click();
  await waitFor(() => {
    const tone = doc.querySelector('[data-el="status"]').dataset.tone;
    return !mill.dataset.busy && (alerted() || tone === 'ok' || tone === 'warn');
  }, { timeout: 40000, what: () => 'the pack: ' + describe(mill) });
  equal(doc.querySelector('[data-el="status"]').textContent, 'Ready', 'the sample pulps: ' + describe(mill));
  const dump = doc.querySelector('[data-el="dump"]').textContent;
  ok(doc.querySelectorAll(DUMP_LINE_SELECTOR).length >= 20, 'the dump is drawn');
  ok(/storm surge/.test(dump) && !/endobj/.test(dump), 'the sample PDF was extracted to text in the tab');
}, { timeout: 75000 });

/**
 * A one-page PDF whose content stream shows a number with Tj, where a string
 * belongs. The PDF parser panics on it, as it does on some damaged real PDFs.
 */
function panickingPdf(win) {
  const stream = 'BT /F1 12 Tf 72 720 Td 42 Tj ET\n';
  const objs = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
    '<< /Length ' + stream.length + ' >>\nstream\n' + stream + 'endstream',
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ];
  let body = '%PDF-1.4\n';
  const at = [];
  objs.forEach((obj, i) => {
    at.push(body.length);
    body += (i + 1) + ' 0 obj\n' + obj + '\nendobj\n';
  });
  const xref = body.length;
  body += 'xref\n0 ' + (objs.length + 1) + '\n0000000000 65535 f \n' + at.map((n) => String(n).padStart(10, '0') + ' 00000 n \n').join('');
  body += 'trailer\n<< /Size ' + (objs.length + 1) + ' /Root 1 0 R >>\nstartxref\n' + xref + '\n%%EOF\n';
  return new win.File([body], 'broken.pdf', { type: 'application/pdf' });
}

async function pulp(doc, mill) {
  doc.querySelector('[data-el="pulpkey"]').click();
  await waitFor(() => {
    const tone = doc.querySelector('[data-el="status"]').dataset.tone;
    return !mill.dataset.busy && (tone === 'ok' || tone === 'warn' || !doc.querySelector('[data-el="alert"]').hidden);
  }, { timeout: 40000, what: () => 'the pack: ' + describe(mill) });
}

test('browser mill: a PDF whose parser panics is noted, and the rest of a drop pulps', async () => {
  const doc = await open('/mill/');
  const win = doc.defaultView;
  const mill = doc.getElementById('mill');
  await waitFor(() => doc.querySelector('.mill-empty [data-act="sample"]'), { timeout: 10000, what: () => 'the mill: ' + describe(mill) });
  const dt = new win.DataTransfer();
  dt.items.add(new win.File(['fn main() {}\n'], 'main.rs'));
  dt.items.add(panickingPdf(win));
  win.dispatchEvent(new win.DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
  await waitFor(() => !mill.dataset.busy && mill.querySelectorAll('.mill-row[data-id]').length === 2, {
    timeout: 20000, what: () => 'the dropped files: ' + describe(mill),
  });
  await pulp(doc, mill);
  equal(doc.querySelector('[data-el="status"]').textContent, '1 issue', describe(mill));
  ok(mill.querySelector('.mill-row.is-flag[data-id="broken.pdf"]'), 'the PDF row is flagged');
  const dump = doc.querySelector('[data-el="dump"]').textContent;
  match(dump, /\[error extracting broken\.pdf: extractor panicked: unexpected Tj operand/, 'the note in the dump');
  match(dump, /fn main\(\) \{\}/, 'the rest of the drop is in the dump');
}, { timeout: 60000 });

test('browser mill: a crash, a panic, or a stack overflow spends the instance it hit, in each engine\'s words', async () => {
  const { isPoisoned } = await import('/mill/worker.js');
  ok(isPoisoned(new RangeError('Maximum call stack size exceeded')), 'a stack overflow in Chrome or Safari');
  ok(isPoisoned(Object.assign(new Error('too much recursion'), { name: 'InternalError' })), 'a stack overflow in Firefox');
  ok(isPoisoned(new WebAssembly.RuntimeError('unreachable')), 'a trap');
  ok(isPoisoned(Object.assign(new Error('boom'), { name: 'PulpPanic' })), 'a panic');
  ok(!isPoisoned(new TypeError('the packer failed')), 'an error the packer returned');
  ok(!isPoisoned(null), 'nothing thrown');
});

/**
 * Start the frame's workers, nested extractors included, from a wrapper that
 * runs `setup` (lines of a module) before worker.js, so it sees each message
 * before the worker does.
 */
function wrapWorkers(win, setup) {
  const lines = [
    'const wrap = self.location.href;',
    'const Real = self.Worker;',
    'if (Real) self.Worker = function (url, options) { return new Real(wrap, options); };',
  ].concat(setup);
  const script = (text) => win.URL.createObjectURL(new win.Blob([text], { type: 'text/javascript' }));
  const wrap = script('import ' + JSON.stringify(script(lines.join('\n'))) + ';\nimport ' + JSON.stringify(win.location.origin + '/mill/worker.js') + ';\n');
  const Real = win.Worker;
  win.Worker = function (url, options) {
    return new Real(wrap, options);
  };
}

/**
 * Wrap the frame's workers so an extractor that reads the file `name`
 * recurses without end, as a parser does on a document that draws itself,
 * and runs out of stack.
 */
function overflowOn(win, name) {
  wrapWorkers(win, [
    'self.addEventListener("message", (ev) => {',
    '  const msg = ev.data;',
    '  if (!msg || msg.type !== "extract" || !msg.payload) return;',
    '  for (const f of msg.payload.files || []) {',
    '    if (f.relative !== ' + JSON.stringify(name) + ') continue;',
    '    const deeper = (n) => deeper(n + 1) + 1;',
    '    Object.defineProperty(f, "file", { get: () => deeper(0) });',
    '  }',
    '});',
  ]);
}

test('browser mill: a parser that runs out of stack is noted, and the rest of a drop pulps', async () => {
  const doc = await open('/mill/');
  const win = doc.defaultView;
  const mill = doc.getElementById('mill');
  await waitFor(() => doc.querySelector('.mill-empty [data-act="sample"]'), { timeout: 10000, what: () => 'the mill: ' + describe(mill) });
  overflowOn(win, 'deep.txt');
  const dt = new win.DataTransfer();
  dt.items.add(new win.File(['fn main() {}\n'], 'main.rs'));
  dt.items.add(new win.File(['a page that draws itself\n'], 'deep.txt'));
  win.dispatchEvent(new win.DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
  await waitFor(() => !mill.dataset.busy && mill.querySelectorAll('.mill-row[data-id]').length === 2, {
    timeout: 20000, what: () => 'the dropped files: ' + describe(mill),
  });
  await pulp(doc, mill);
  equal(doc.querySelector('[data-el="status"]').textContent, '1 issue', describe(mill));
  ok(mill.querySelector('.mill-row.is-flag[data-id="deep.txt"]'), 'the file is flagged');
  const dump = doc.querySelector('[data-el="dump"]').textContent;
  match(dump, /\[error extracting deep\.txt: extractor ran out of stack\]/, 'the note in the dump');
  match(dump, /fn main\(\) \{\}/, 'the rest of the drop is in the dump');
}, { timeout: 60000 });

/**
 * Wrap the frame's workers so each extractor reports the files it is asked
 * to extract. Returns the names, which grow as packs run.
 */
function countExtractions(win) {
  const channel = 'pulp-extract-' + Math.random().toString(36).slice(2);
  const names = [];
  const listen = new BroadcastChannel(channel);
  listen.onmessage = (ev) => names.push(...ev.data);
  wrapWorkers(win, [
    'const report = new BroadcastChannel(' + JSON.stringify(channel) + ');',
    'self.addEventListener("message", (ev) => {',
    '  const msg = ev.data;',
    '  if (msg && msg.type === "extract" && msg.payload) report.postMessage(msg.payload.files.map((f) => f.relative));',
    '});',
  ]);
  return { names, close: () => listen.close() };
}

/** Write `text` to `path` under the folder handle `top`, making its folders. */
async function writeFile(top, path, text) {
  const parts = path.split('/');
  let dir = top;
  for (const part of parts.slice(0, -1)) dir = await dir.getDirectoryHandle(part, { create: true });
  const handle = await dir.getFileHandle(parts[parts.length - 1], { create: true });
  const out = await handle.createWritable();
  await out.write(text);
  await out.close();
}

/**
 * A folder handle that records, in `listed`, the path of every folder a walk
 * lists, and otherwise answers as `handle` does.
 */
function recording(handle, listed, path = '') {
  if (handle.kind !== 'directory') return handle;
  return {
    kind: 'directory',
    name: handle.name,
    async *values() {
      listed.push(path);
      for await (const child of handle.values()) yield recording(child, listed, path ? path + '/' + child.name : child.name);
    },
  };
}

/** A folder `name` of `{ path: text }` files in the frame's private file system, as a picker would hand it over. */
async function privateFolder(win, name, files) {
  const root = await win.navigator.storage.getDirectory();
  await root.removeEntry(name, { recursive: true }).catch(() => {});
  const top = await root.getDirectoryHandle(name, { create: true });
  for (const [path, text] of Object.entries(files)) await writeFile(top, path, text);
  return { top, remove: () => root.removeEntry(name, { recursive: true }).catch(() => {}) };
}

test('browser mill: a picked folder is walked past node_modules, a second pulp reuses every file, and an edit is read as it is now', async () => {
  const doc = await open('/mill/');
  const win = doc.defaultView;
  const mill = doc.getElementById('mill');
  const browse = await waitFor(() => doc.querySelector('.mill-empty [data-act="browse"]'), { timeout: 10000, what: () => 'the mill: ' + describe(mill) });
  const folder = await privateFolder(win, 'proj', {
    'src/lib.rs': 'pub fn tide() -> u32 {\n    1\n}\n',
    'src/main.rs': 'fn main() {}\n',
    'README.md': '# proj\n',
    'node_modules/pkg/index.js': 'module.exports = 1;\n',
  });
  const extracted = countExtractions(win);
  const listed = [];
  try {
    // The page asks the browser's folder picker for the folder.
    win.showDirectoryPicker = async () => recording(folder.top, listed);
    browse.click();
    await waitFor(() => !mill.dataset.busy && mill.querySelectorAll('.mill-row[data-id]').length === 3, {
      timeout: 20000, what: () => 'the scan: ' + describe(mill),
    });
    ok(!mill.querySelector('.mill-row[data-id^="proj/node_modules"]'), 'node_modules is left out');
    same(listed.sort(), ['', 'src'], 'the walk never opens node_modules');
    const dump = () => doc.querySelector('[data-el="dump"]').textContent;
    const status = () => doc.querySelector('[data-el="status"]').textContent;

    await pulp(doc, mill);
    await sleep(100);
    equal(status(), 'Ready', describe(mill));
    same(extracted.names.slice().sort(), ['README.md', 'src/lib.rs', 'src/main.rs'], 'the first pulp extracts every file');
    const first = dump();

    extracted.names.length = 0;
    await pulp(doc, mill);
    await sleep(100);
    same(extracted.names, [], 'a second pulp of an unchanged folder extracts nothing');
    equal(dump(), first, 'and gives the same dump');

    // Edited in place after the pick: read as it is now, not flagged.
    await sleep(20);
    await writeFile(folder.top, 'src/lib.rs', 'pub fn tide() -> u32 {\n    2\n}\n');
    extracted.names.length = 0;
    await pulp(doc, mill);
    await sleep(100);
    same(extracted.names, ['src/lib.rs'], 'only the edited file is extracted again');
    equal(status(), 'Ready', 'nothing is flagged: ' + describe(mill));
    match(dump(), /u32 \{\n\s*2\n\}/, 'the dump holds the edit');
    match(dump(), /fn main\(\) \{\}/, 'and the files taken from the last dump');

    // Gone since the scan: that file cannot be read at all, so it is flagged.
    const root = await win.navigator.storage.getDirectory();
    await (await root.getDirectoryHandle('proj')).removeEntry('README.md');
    await pulp(doc, mill);
    equal(status(), '1 issue', describe(mill));
    ok(mill.querySelector('.mill-row.is-flag[data-id="proj/README.md"]'), 'the deleted file is flagged');
    match(dump(), /\[changed since scan: no longer there\]/, 'and noted as gone, as pulp ui notes it');
  } finally {
    extracted.close();
    await folder.remove();
  }
}, { timeout: 90000 });

test('browser mill: a dismissed folder picker offers the folder input next, and a refused one at once', async () => {
  const doc = await open('/mill/');
  const win = doc.defaultView;
  const mill = doc.getElementById('mill');
  const browse = await waitFor(() => doc.querySelector('.mill-empty [data-act="browse"]'), { timeout: 10000, what: () => 'the mill: ' + describe(mill) });
  const input = doc.getElementById('dirInput');
  let opened = 0;
  input.click = () => { opened++; };
  let asked = 0;
  let refusal = 'AbortError';
  win.showDirectoryPicker = async () => {
    asked++;
    throw new win.DOMException('The user aborted a request.', refusal);
  };
  // Chrome answers a closed picker and a declined permission prompt alike.
  browse.click();
  await waitFor(() => asked === 1 && !mill.dataset.busy, { what: 'the folder picker' });
  await sleep(50);
  equal(opened, 0, 'a dismissed picker is a cancel');
  browse.click();
  await waitFor(() => opened === 1, { what: 'the folder input' });
  equal(asked, 1, 'the next Choose folder offers the folder input');
  // A picker refused outright, say in a frame, falls back at once.
  refusal = 'SecurityError';
  browse.click();
  await waitFor(() => asked === 2 && opened === 2, { what: () => 'the folder input after a refusal: asked ' + asked + ', opened ' + opened });
}, { timeout: 20000 });

test('browser mill: a picked folder lists its files from inside it, as pulp ui does', async () => {
  const doc = await open('/mill/');
  const mill = doc.getElementById('mill');
  const sample = await waitFor(() => doc.querySelector('.mill-empty [data-act="sample"]'), { timeout: 10000, what: () => 'the mill: ' + describe(mill) });
  sample.click();
  await waitFor(() => !mill.dataset.busy && mill.querySelector('.mill-row[data-id]'), { timeout: 20000, what: () => 'the sample scan: ' + describe(mill) });
  const dirs = [...mill.querySelectorAll('.mill-row[data-dir]')].map((row) => row.dataset.dir);
  ok(!dirs.includes('tides'), 'no row for the folder itself: ' + dirs.join(', '));
  ok(dirs.includes('src') && mill.querySelector('.mill-row[data-id="tides/src/lib.rs"]'), 'rows start inside the folder and ids keep it: ' + dirs.join(', '));
  const copied = [];
  const clip = doc.defaultView.navigator.clipboard;
  clip.writeText = async (text) => { copied.push(String(text)); };
  clip.write = async (items) => { copied.push(await (await items[0].getType('text/plain')).text()); };
  doc.querySelector('[data-el="outkeys"] [data-act="tree"]').click();
  await waitFor(() => copied.length, { timeout: 10000, what: 'the copied directory map' });
  match(copied[0], /^<document_tree>\ntides\/\n├── Cargo\.toml\n/, 'the map is named after the folder, as the dump is');
}, { timeout: 40000 });

run();
