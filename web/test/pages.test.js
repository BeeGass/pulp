// Page tests: load the site as it deploys (the landing page at / and the
// browser mill at /mill/, both served from site/ by `cargo xtask ui-test`) in
// same-origin frames and use them the way a visitor would.

import { equal, ok, run, test, waitFor } from './harness.js';

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

run();
