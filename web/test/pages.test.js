// Page tests: load the site as it deploys (the landing page at / and the
// browser mill at /mill/, both served from site/ by `cargo xtask ui-test`) in
// same-origin frames and use them the way a visitor would.

import { equal, match, ok, run, test, waitFor } from './harness.js';

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
 * Start the frame's workers from a wrapper around worker.js in which an
 * extractor that reads the file `name` recurses without end, as a parser does
 * on a document that draws itself, and runs out of stack.
 */
function overflowOn(win, name) {
  const setup = [
    'const wrap = self.location.href;',
    'const Real = self.Worker;',
    'if (Real) self.Worker = function (url, options) { return new Real(wrap, options); };',
    'self.addEventListener("message", (ev) => {',
    '  const msg = ev.data;',
    '  if (!msg || msg.type !== "extract" || !msg.payload) return;',
    '  for (const f of msg.payload.files || []) {',
    '    if (f.relative !== ' + JSON.stringify(name) + ') continue;',
    '    const deeper = (n) => deeper(n + 1) + 1;',
    '    Object.defineProperty(f, "file", { get: () => deeper(0) });',
    '  }',
    '});',
  ].join('\n');
  const script = (text) => win.URL.createObjectURL(new win.Blob([text], { type: 'text/javascript' }));
  const wrap = script('import ' + JSON.stringify(script(setup)) + ';\nimport ' + JSON.stringify(win.location.origin + '/mill/worker.js') + ';\n');
  const Real = win.Worker;
  win.Worker = function (url, options) {
    return new Real(wrap, options);
  };
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
