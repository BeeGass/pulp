import init, { pack_files, scan_files } from './pkg/pulp_wasm.js';

let ready;
function ensure() {
  if (!ready) {
    ready = init().catch((err) => {
      ready = null;
      throw err;
    });
  }
  return ready;
}

self.onmessage = async (ev) => {
  const msg = ev.data || {};
  const id = msg.id;
  try {
    if (msg.type === 'cancel') {
      self.postMessage({ ok: false, id, error: 'cancelled' });
      return;
    }
    await ensure();
    if (msg.type === 'pack') {
      const result = pack_files(msg.payload);
      self.postMessage({ ok: true, id, result });
    } else if (msg.type === 'scan') {
      const result = scan_files(msg.payload);
      self.postMessage({ ok: true, id, result });
    } else {
      self.postMessage({ ok: false, id, error: 'unknown message' });
    }
  } catch (e) {
    self.postMessage({ ok: false, id, error: String(e && e.message || e) });
  }
};
