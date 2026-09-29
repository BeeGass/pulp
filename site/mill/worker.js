// Packs off the main thread so the mill stays responsive. The page cancels a
// pack by terminating this worker; the extracted files stay here, so the page
// can redraw the dump in another format or ask for the full dump.
import init, { pack_files, render_result, artifact, artifact_as, drop_result, preview_file } from './pkg/pulp_wasm.js';

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

function run(type, payload) {
  switch (type) {
    case 'pack': return pack_files(payload);
    case 'render': return render_result(payload);
    case 'artifact': return artifact(payload.id);
    case 'artifact_as': return artifact_as(payload);
    case 'preview': return preview_file(payload);
    case 'drop': return drop_result(payload.id);
    default: throw new Error('unknown message ' + type);
  }
}

self.onmessage = async (ev) => {
  const msg = ev.data || {};
  const id = msg.id;
  try {
    await ensure();
    self.postMessage({ ok: true, id, result: run(msg.type, msg.payload || {}) });
  } catch (e) {
    self.postMessage({ ok: false, id, error: String((e && e.message) || e) });
  }
};
