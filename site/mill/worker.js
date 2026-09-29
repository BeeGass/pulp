// The browser mill's worker. The page starts one per pulp (the pack worker),
// one for scans and trees, and one for previews. A pack worker starts a nested
// worker of its own, the extractor, to turn the ticked files into text in
// batches. A parser that panics, traps, runs out of stack, or hangs takes down
// only the extractor: the pack worker notes that file in the dump and carries
// on with a fresh one, as `pulp ui` does with its child processes. The pack
// worker keeps the result, so the page can redraw the dump in another format
// or ask for the full dump, and the page cancels a pack by terminating it.
//
// The page imports this module too: for the build it shipped with, and to run
// the same code on its own thread when it cannot start workers.
import init, * as pulp from './pkg/pulp_wasm.js';

/**
 * The packer build this file ships with: the FNV-1a hash of
 * pkg/pulp_wasm_bg.wasm, which a test in crates/pulp-wasm keeps it equal to.
 * A page hands its compiled packer only to a worker of the same build; a
 * worker from a later deploy loads the packer it shipped with.
 */
export const BUILD = '4a44d71575539bea';

/** Leading bytes read from a file whose name does not say what it holds. */
const HEAD_BYTES = 8192;
/** Files read at once. */
const READ_POOL = 16;
/** Largest batch the extractor gets: files and summed bytes. */
const BATCH_FILES = 256;
const BATCH_BYTES = 16 * 1024 * 1024;
/** A full dump longer than this comes back as a Blob; a JS string cannot hold much more. */
export const STRING_BYTES = 256 * 1024 * 1024;

const inWorker = typeof WorkerGlobalScope !== 'undefined' && self instanceof WorkerGlobalScope;

/** Fetch and compile the packer of this build. */
async function compilePacker() {
  const res = await fetch(new URL('./pkg/pulp_wasm_bg.wasm', import.meta.url));
  if (!res.ok) throw new Error('could not fetch the packer (' + res.status + ')');
  const wasm = (res.headers.get('content-type') || '').split(';')[0].trim() === 'application/wasm';
  if (wasm && typeof WebAssembly.compileStreaming === 'function') return WebAssembly.compileStreaming(res);
  return WebAssembly.compile(await res.arrayBuffer());
}

/** The compiled packer this thread runs: one of this build it was handed, else its own. */
let compiled = null;
let loading = null;

/**
 * Load the packer on this thread once, and return its compiled module. A
 * worker that cannot load its own packer, or link the one it was handed, is
 * from another deploy than the page that started it (`stale`), and only a
 * reload brings the two together again.
 */
export function ensure() {
  if (!loading) {
    loading = (async () => {
      const handed = !!compiled;
      try {
        if (!compiled) compiled = await compilePacker();
        await init({ module_or_path: compiled });
        return compiled;
      } catch (err) {
        loading = null;
        const error = err instanceof Error ? err : new Error(String(err));
        if (inWorker && (!handed || error.name === 'LinkError')) error.stale = true;
        throw error;
      }
    })();
  }
  return loading;
}

/**
 * A panic, a trap, or a parser that ran out of stack leaves the instance that
 * raised it unusable. Stack exhaustion in the packer is a RangeError in
 * Chrome's and Safari's workers, an InternalError ("too much recursion") in
 * Firefox, and a RuntimeError on some threads.
 */
export function isPoisoned(err) {
  if (!err || typeof err !== 'object') return false;
  if (err.poisoned === true) return true;
  return err.name === 'PulpPanic' || err.name === 'RuntimeError' || err.name === 'RangeError' || err.name === 'InternalError';
}

/** How `pulp` words a parser that brought its instance down. */
function crashText(err) {
  if (err && err.name === 'PulpPanic') return 'extractor panicked: ' + (err.payload || err.message);
  const why = errorText(err);
  if (why === 'unreachable') return 'extractor ran out of memory or crashed';
  if (/call stack size|too much recursion/i.test(why)) return 'extractor ran out of stack';
  return 'extractor crashed: ' + why;
}

function errorText(err) {
  return err && err.message ? err.message : String(err);
}

async function mapPool(items, limit, fn) {
  const out = new Array(items.length);
  let next = 0;
  async function run() {
    while (next < items.length) {
      const i = next++;
      out[i] = await fn(items[i], i);
    }
  }
  await Promise.all(Array.from({ length: Math.min(limit, Math.max(1, items.length)) }, run));
  return out;
}

/**
 * Why a chosen file could not be read. Browsers keep a snapshot of each chosen
 * file and refuse to read it once it changes, moves, or loses its read
 * permission, each with its own error: Chrome's NotReadableError or
 * NotFoundError, Safari's NotFoundError, Firefox's AbortError.
 */
function readFailure(err) {
  const name = err && err.name;
  if (name === 'NotReadableError' || name === 'NotFoundError' || name === 'AbortError' || name === 'NotAllowedError') {
    return { reason: 'changed', message: '' };
  }
  return { reason: 'error', message: 'the browser could not read the file' + (err && err.message ? ': ' + err.message : '') };
}

/** The note for a file the page's own thread leaves alone: nothing there could stop a parser that hangs. */
const NO_WORKER = { reason: 'no_worker', message: '' };

/** Leading bytes; a small file is read whole, which is far faster than a slice. */
async function head(file) {
  const part = file.size > HEAD_BYTES ? file.slice(0, HEAD_BYTES) : file;
  return new Uint8Array(await part.arrayBuffer());
}

/**
 * Classify a grant, `{ entries: [[relative, File]], root, hidden, archives }`:
 * the files `scan_keep` kept, under the root it found. Files whose names do
 * not decide their kind are sniffed; one that cannot be read is classified by
 * name, as `pulp ui` does.
 */
export async function scanGrant(payload) {
  await ensure();
  const entries = payload.entries || [];
  const opts = { hidden: !!payload.hidden, archives: !!payload.archives, exclude: [], root: payload.root || '' };
  const files = entries.map(([relative, file]) => ({ relative, size: file.size }));
  const unknown = pulp.scan_unknown(Object.assign({ files }, opts));
  await mapPool(Array.from(unknown), 24, async (i) => {
    try {
      files[i].head = await head(entries[i][1]);
    } catch (_) {
      // Classified by name.
    }
  });
  return pulp.scan_files(Object.assign({ files }, opts));
}

/**
 * Preview one chosen file, `{ relative, kind, file, ...settings }`, where
 * `kind` is the scan's. A file that cannot be read gets its reason. On the
 * page's own thread, a file whose parser could hang gets a note instead.
 */
export async function previewGrant(payload) {
  await ensure();
  const { file } = payload;
  const input = Object.assign({}, payload, { size: file.size });
  delete input.file;
  if (!inWorker && pulp.needs_worker(payload.kind || '', !!payload.archives)) {
    input.failure = NO_WORKER;
    return pulp.preview_file(input);
  }
  try {
    input.bytes = new Uint8Array(await file.arrayBuffer());
  } catch (err) {
    input.failure = readFailure(err);
  }
  return pulp.preview_file(input);
}

/**
 * Read and extract one batch, `{ root, settings, files: [{ id, relative, kind, file }] }`:
 * its JSON bytes for `pack_add`.
 */
async function extractBatch(payload) {
  await ensure();
  const files = await mapPool(payload.files, READ_POOL, async (f) => {
    const one = { id: f.id, relative: f.relative, kind: f.kind, size: f.file.size };
    try {
      one.bytes = new Uint8Array(await f.file.arrayBuffer());
    } catch (err) {
      one.failure = readFailure(err);
    }
    return one;
  });
  return pulp.extract_files(Object.assign({}, payload.settings, { root: payload.root, files }));
}

/** Heavy files and archives on their own; the rest in batches by count and bytes. */
function batches(todo) {
  const out = [];
  let batch = [];
  let bytes = 0;
  const flush = () => {
    if (batch.length) out.push(batch);
    batch = [];
    bytes = 0;
  };
  for (const f of todo) {
    if (f.alone) {
      flush();
      out.push([f]);
      continue;
    }
    if (batch.length >= BATCH_FILES || (batch.length && bytes + f.file.size > BATCH_BYTES)) flush();
    batch.push(f);
    bytes += f.file.size;
  }
  flush();
  return out;
}

/**
 * Runs batches in a nested worker and replaces it whenever one is lost to a
 * panic, a trap, a stack overflow, a crash, or the time limit. Where nested
 * workers cannot start, it extracts in this instance, and a panic fails the
 * pack; on the page's own thread, a file whose parser could hang is noted
 * instead of parsed.
 */
class Extractor {
  constructor(settings, root, nested) {
    this.settings = settings;
    this.root = root;
    this.nested = !!nested && typeof Worker !== 'undefined';
    this.worker = null;
    this.ready = false;
    this.seq = 0;
    this.limit = pulp.extract_timeout_ms();
  }

  close() {
    if (this.worker) this.worker.terminate();
    this.worker = null;
  }

  /** The text of `batch` in parts, `[{ files, json }]`, each part's JSON bytes for `pack_add`. */
  async run(batch) {
    if (!inWorker) {
      const risky = batch.filter((f) => pulp.needs_worker(f.kind, !!this.settings.archives));
      if (risky.length) {
        const rest = batch.filter((f) => !risky.includes(f));
        return [this.note(risky, NO_WORKER)].concat(rest.length ? await this.run(rest) : []);
      }
    }
    const payload = { root: this.root, settings: this.settings, files: batch };
    if (this.nested) {
      try {
        return [{ files: batch, json: await this.once(payload, batch.length === 1 && batch[0].heavy) }];
      } catch (err) {
        if (err.unavailable) this.nested = false;
        else if (err.lost) return this.recover(batch, err);
        else throw err;
      }
    }
    try {
      return [{ files: batch, json: await extractBatch(payload) }];
    } catch (err) {
      if (!isPoisoned(err)) throw err;
      // Here the parser shares this instance, so nothing can set the file aside.
      const which = batch.length === 1 ? batch[0].relative : 'one of ' + batch.length + ' files';
      throw Object.assign(new Error(crashText(err) + ' on ' + which +
        '. This browser could not give the parser a worker of its own, so the pack stopped. Untick that file, reload the page, and pulp again.'), { poisoned: true });
    }
  }

  /** A lost batch again, one file at a time; the file that brings its extractor down is noted. */
  async recover(batch, err) {
    if (batch.length > 1) {
      const parts = [];
      for (const one of batch) parts.push(...(await this.run([one])));
      return parts;
    }
    return [this.note(batch, err.failure || { reason: 'error', message: err.message })];
  }

  /** A part that notes each of `files` with `failure`, from this instance. */
  note(files, failure) {
    return {
      files,
      json: pulp.extract_files(Object.assign({}, this.settings, {
        root: this.root,
        files: files.map((f) => ({ id: f.id, relative: f.relative, kind: f.kind, size: f.file.size, failure })),
      })),
    };
  }

  spawn() {
    const worker = new Worker(new URL(import.meta.url), { type: 'module' });
    this.worker = worker;
    this.ready = false;
    worker.addEventListener('message', (ev) => {
      if (ev.data && ev.data.ready && this.worker === worker) this.ready = true;
    });
    return worker;
  }

  once(payload, heavy) {
    return new Promise((resolve, reject) => {
      let worker;
      try {
        worker = this.worker || this.spawn();
      } catch (err) {
        reject(Object.assign(err, { unavailable: true }));
        return;
      }
      const id = ++this.seq;
      let timer = 0;
      const settle = () => {
        worker.removeEventListener('message', onMessage);
        worker.removeEventListener('error', onError);
        if (timer) clearTimeout(timer);
      };
      // An extractor that never loaded means nested workers do not run here.
      const lose = (failure) => {
        const unavailable = !this.ready;
        settle();
        worker.terminate();
        if (this.worker === worker) this.worker = null;
        reject(Object.assign(new Error(failure.message || failure.reason), unavailable ? { unavailable: true } : { lost: true, failure }));
      };
      const onMessage = (ev) => {
        const msg = ev.data || {};
        if (msg.id !== id) return;
        if (msg.ok) {
          settle();
          resolve(msg.result);
        } else if (msg.poisoned) {
          this.ready = true;
          lose({ reason: 'error', message: msg.crash || msg.error });
        } else {
          settle();
          reject(Object.assign(new Error(msg.error || 'the extractor failed'), { stale: !!msg.stale }));
        }
      };
      const onError = (ev) => {
        if (ev.preventDefault) ev.preventDefault();
        lose({ reason: 'error', message: 'the extractor stopped' + (ev.message ? ': ' + ev.message : '') });
      };
      worker.addEventListener('message', onMessage);
      worker.addEventListener('error', onError);
      if (heavy) {
        timer = setTimeout(() => {
          this.ready = true;
          lose({ reason: 'timeout', message: '' });
        }, this.limit);
      }
      worker.postMessage({ type: 'extract', id, payload, module: compiled, build: BUILD });
    });
  }
}

/**
 * Add a part to the pack. A part the pack cannot take is extracted again a
 * file at a time, and a file whose text it still cannot take is noted.
 */
async function addPart(extractor, part) {
  try {
    pulp.pack_add(part.json);
  } catch (err) {
    if (isPoisoned(err)) throw err;
    if (part.files.length > 1) {
      for (const f of part.files) {
        for (const one of await extractor.run([f])) await addPart(extractor, one);
      }
      return;
    }
    pulp.pack_add(extractor.note(part.files, { reason: 'error', message: 'its text could not be added to the dump: ' + errorText(err) }).json);
  }
}

/**
 * Pack the ticked files, `{ files: [{ id, relative, kind, file }], truncated,
 * ...settings }`, where `kind` is the scan's kind for each file and
 * `truncated` says the scan was cut short, reporting `progress(done, total)`
 * in files. `nested` extracts in a nested worker; `stopped()` is checked
 * between batches.
 */
export async function packGrant(payload, progress, nested, stopped) {
  await ensure();
  const settings = Object.assign({}, payload);
  delete settings.files;
  delete settings.truncated;
  const ticked = payload.files || [];
  const plan = pulp.pack_begin(Object.assign({}, settings, {
    truncated: !!payload.truncated,
    files: ticked.map((f) => ({ id: f.id, relative: f.relative, size: f.file.size, kind: f.kind })),
  }));
  const todo = plan.files.map((p) => Object.assign({}, p, { file: ticked[p.index].file }));
  const total = todo.length;
  let done = 0;
  progress(done, total);
  const extractor = new Extractor(settings, plan.root, nested);
  try {
    for (const batch of batches(todo)) {
      if (stopped && stopped()) throw Object.assign(new Error('cancelled'), { cancelled: true });
      for (const part of await extractor.run(batch)) await addPart(extractor, part);
      done += batch.length;
      progress(done, total);
    }
  } finally {
    extractor.close();
  }
  return pulp.pack_finish();
}

/**
 * The full dump of a stored result, `{ result_id, format, no_tree }`: a
 * string, or a Blob when the dump is too long for one.
 */
export async function dumpOf(input) {
  await ensure();
  const parts = [];
  const total = pulp.artifact_chunks(input, (chunk) => {
    parts.push(chunk);
  });
  if (total > STRING_BYTES) return new Blob(parts, { type: 'text/plain;charset=utf-8' });
  const decoder = new TextDecoder();
  let text = '';
  for (const part of parts) text += decoder.decode(part, { stream: true });
  return text + decoder.decode();
}

/* ---------- worker entry ---------- */

/** Set once this instance has panicked, trapped, or run out of stack; it takes no more work. */
let spent = '';

async function run(msg) {
  const payload = msg.payload || {};
  // The pack worker and its extractor pass batches in a form only one build reads.
  if (msg.type === 'extract' && msg.build !== BUILD) {
    throw Object.assign(new Error('The mill was updated while this page was open. Reload the page, then pulp again.'), { stale: true });
  }
  await ensure();
  switch (msg.type) {
    case 'pack_grant':
      return packGrant(payload, (done, total) => self.postMessage({ id: msg.id, progress: [done, total] }), true);
    case 'extract': return extractBatch(payload);
    case 'scan_keep': return pulp.scan_keep(payload);
    case 'scan': return scanGrant(payload);
    case 'tree': return pulp.format_tree(payload);
    case 'preview_grant': return previewGrant(payload);
    case 'render': return pulp.render_result(payload);
    case 'dump': return dumpOf(payload);
    case 'artifact_as': return pulp.artifact_as(payload);
    // Kept for pages loaded before the grant messages existed.
    case 'pack': return pulp.pack_files(payload);
    case 'preview': return pulp.preview_file(payload);
    case 'artifact': return pulp.artifact(payload.id);
    case 'drop': return pulp.drop_result(payload.id);
    default: throw new Error('unknown message ' + msg.type);
  }
}

/** A batch's JSON bytes go over without a copy. */
function transferOf(result) {
  return result instanceof Uint8Array && result.byteOffset === 0 && result.byteLength === result.buffer.byteLength ? [result.buffer] : [];
}

if (inWorker) {
  // Tells whoever started this worker that it loaded, so a worker that fails
  // later is not mistaken for one that cannot start at all.
  self.postMessage({ ready: true });
  self.onmessage = async (ev) => {
    const msg = ev.data || {};
    const id = msg.id;
    // A packer compiled for another build would not fit this worker's glue.
    if (msg.module && msg.build === BUILD && !compiled) compiled = msg.module;
    if (spent) {
      self.postMessage({ ok: false, id, poisoned: true, error: spent, crash: spent });
      return;
    }
    try {
      const result = await run(msg);
      self.postMessage({ ok: true, id, result }, transferOf(result));
    } catch (err) {
      const poisoned = isPoisoned(err);
      if (poisoned) spent = errorText(err);
      self.postMessage({ ok: false, id, poisoned, stale: !!(err && err.stale), error: errorText(err), crash: poisoned ? crashText(err) : '' });
    }
  };
}
