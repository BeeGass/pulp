import init, { pack_files, scan_files } from './pkg/pulp_wasm.js';

let ready;
function ensure() {
  if (!ready) ready = init();
  return ready;
}

self.onmessage = async (ev) => {
  const msg = ev.data || {};
  try {
    await ensure();
    if (msg.type === 'pack') {
      const result = pack_files(msg.payload);
      self.postMessage({ ok: true, result });
    } else if (msg.type === 'scan') {
      const result = scan_files(msg.payload);
      self.postMessage({ ok: true, result });
    } else {
      self.postMessage({ ok: false, error: 'unknown message' });
    }
  } catch (e) {
    self.postMessage({ ok: false, error: String(e && e.message || e) });
  }
};
