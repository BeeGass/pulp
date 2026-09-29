// Test runner for the mill's browser tests. `cargo xtask ui-test` opens every
// web/test/*.test.html page in headless Chrome and reads its #results block:
// one PASS or FAIL line per test, failure detail indented by two spaces, a
// "# ..." summary, and data-status="pass" or "fail" once the run is over.
//
// Load this module on its own before the test module, so a test module that
// fails to load or parse still ends the run with a FAIL line.

const DONE_URL = '/__ui-test/done';
const out = document.getElementById('results');
const tests = [];
const stray = [];
let current = null;
let started = false;
let finished = false;

export class AssertionError extends Error {
  constructor(message) {
    super(message);
    this.name = 'AssertionError';
  }
}

// An uncaught error fails the test that is running. Before the tests start it
// means the page itself is broken, and the run ends at once.
function fault(err) {
  if (current) {
    current.errors.push(err);
  } else if (!started) {
    write('FAIL page setup\n' + detail(err));
    finish(false);
  } else {
    stray.push(err);
  }
}

addEventListener('error', (e) => {
  // Captured so a module script that fails to load is seen; stylesheets and
  // images are not test failures.
  const script = e.target instanceof HTMLScriptElement;
  if (!script && !(e instanceof ErrorEvent)) return;
  fault(script ? new Error(e.target.src + ' did not load') : e.error || new Error(e.message));
}, true);

addEventListener('unhandledrejection', (e) => {
  fault(e.reason instanceof Error ? e.reason : new Error('unhandled rejection: ' + show(e.reason)));
});

/** Register a test. `options.timeout` is in milliseconds. */
export function test(name, fn, options) {
  const timeout = (options && options.timeout) || 4000;
  // The report gives each test one line.
  tests.push({ name: String(name).replace(/\s+/g, ' ').trim(), fn, timeout });
}

/** Run every registered test in order, then report. */
export async function run() {
  if (started) return;
  started = true;
  const t0 = performance.now();
  let passed = 0;
  let failed = 0;
  for (const t of tests) {
    current = { errors: [] };
    let error = null;
    try {
      await within(t.timeout, t.fn);
      // Errors a test causes can surface a task later; charge them to it.
      await flush();
    } catch (err) {
      error = err;
    }
    error = error || current.errors[0] || null;
    current = null;
    if (error) failed++;
    else passed++;
    write((error ? 'FAIL ' : 'PASS ') + t.name + '\n' + (error ? detail(error) : ''));
  }
  await flush();
  if (stray.length) {
    failed++;
    write('FAIL uncaught errors between tests\n' + stray.map(detail).join(''));
  }
  write('# ' + passed + ' passed, ' + failed + ' failed in ' + Math.round(performance.now() - t0) + ' ms\n');
  finish(failed === 0);
}

function finish(pass) {
  if (finished) return;
  finished = true;
  out.dataset.status = pass ? 'pass' : 'fail';
  fetch(DONE_URL, { method: 'POST' }).catch(() => {
    // Opened by hand from another server; there is no gate to release.
  });
}

function write(text) {
  out.append(text);
}

function detail(err) {
  const lines = String((err && err.message) || err).split('\n');
  if (!(err instanceof AssertionError) && err && err.stack) {
    // Where the error came from, minus this runner's own frames.
    const frames = err.stack.split('\n').slice(1).map((line) => line.trim());
    lines.push(...frames.filter((line) => line.startsWith('at ') && !line.includes('/web/test/harness.js')).slice(0, 2));
  }
  return lines.map((line) => '  ' + line + '\n').join('');
}

function within(ms, fn) {
  let timer = 0;
  const limit = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new AssertionError('timed out after ' + ms + ' ms')), ms);
  });
  return Promise.race([Promise.resolve().then(fn), limit]).finally(() => clearTimeout(timer));
}

/* ---------- assertions ---------- */

export function show(value) {
  if (value instanceof Element) {
    const bits = ['data-act', 'data-el', 'data-lang', 'data-dir', 'data-id']
      .filter((name) => value.hasAttribute(name))
      .map((name) => name + '="' + value.getAttribute(name) + '"');
    return '<' + value.tagName.toLowerCase() + (bits.length ? ' ' + bits.join(' ') : '') + '>';
  }
  if (value === undefined) return 'undefined';
  try {
    return JSON.stringify(value);
  } catch (_) {
    return String(value);
  }
}

function prefix(message) {
  return message ? message + ': ' : '';
}

export function ok(value, message) {
  if (!value) throw new AssertionError(message || 'expected a truthy value, got ' + show(value));
}

export function equal(actual, expected, message) {
  if (!Object.is(actual, expected)) {
    throw new AssertionError(prefix(message) + 'expected ' + show(expected) + ', got ' + show(actual));
  }
}

/** Compare plain data (arrays, objects, strings) by value. */
export function same(actual, expected, message) {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new AssertionError(prefix(message) + 'expected ' + b + ', got ' + a);
}

export function match(actual, pattern, message) {
  if (!pattern.test(String(actual))) {
    throw new AssertionError(prefix(message) + 'expected ' + show(String(actual)) + ' to match ' + pattern);
  }
}

/* ---------- time ---------- */

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Let every pending promise callback run. */
export function flush() {
  return sleep(0);
}

/**
 * Poll `fn` until it returns something truthy and return that. `what` (a string
 * or a function returning one) describes the wait when it times out.
 */
export async function waitFor(fn, options) {
  const { timeout = 2000, what = 'a condition' } = options || {};
  const end = performance.now() + timeout;
  let lastError = null;
  for (;;) {
    try {
      const value = fn();
      if (value) return value;
    } catch (err) {
      lastError = err;
    }
    if (performance.now() > end) {
      const about = typeof what === 'function' ? what() : what;
      throw new AssertionError('timed out after ' + timeout + ' ms waiting for ' + about +
        (lastError ? ' (last error: ' + lastError.message + ')' : ''));
    }
    await sleep(20);
  }
}
