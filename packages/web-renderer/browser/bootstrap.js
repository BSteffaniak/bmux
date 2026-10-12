import init, { CanvasRenderer } from '/bmux_web_renderer.js';

// Capability bootstrap is explicit and memory-only. Remove it before any I/O.
let token = new URLSearchParams(location.hash.slice(1)).get('token');
history.replaceState(null, '', location.pathname + location.search);
const status = document.getElementById('status');
const MAX_JSON_BYTES = 16 * 1024 * 1024;
let stopped = false;
let activeRequest;
addEventListener('pagehide', () => {
  stopped = true;
  token = null;
  activeRequest?.abort();
});

async function boundedText(response) {
  const length = response.headers.get('Content-Length');
  if (length !== null && Number(length) > MAX_JSON_BYTES) throw new Error('frame too large');
  const reader = response.body.getReader();
  const chunks = [];
  let size = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.length;
      if (size > MAX_JSON_BYTES) throw new Error('frame too large');
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel();
    throw error;
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
  return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
}

function transportRevision(value) {
  if (typeof value !== 'string' || !/^[1-9][0-9]{0,19}$/.test(value)) {
    throw new Error('invalid transport revision');
  }
  const parsed = BigInt(value);
  if (parsed > 18446744073709551615n) throw new Error('transport revision exceeds u64');
  return parsed;
}

async function run() {
  if (!token || token.length > 4096 || /[\r\n]/.test(token)) {
    status.textContent = 'Missing or invalid bootstrap token.';
    token = null;
    return;
  }
  await init('/bmux_web_renderer_bg.wasm');
  const renderer = new CanvasRenderer('bmux', 10, 20);
  // Transport publications and native presentation revisions are independent.
  let pollCursor = null;
  let transportHighWater = null;
  let generation = null;
  try {
    while (!stopped) {
      activeRequest = new AbortController();
      const timeout = setTimeout(() => activeRequest.abort(), 35000);
      try {
        const requestCursor = pollCursor;
        const path = requestCursor === null ? '/snapshot' : `/snapshot?after=${encodeURIComponent(requestCursor)}`;
        const response = await fetch(path, {
          headers: { Authorization: `Bearer ${token}` },
          cache: 'no-store', credentials: 'omit', redirect: 'error',
          signal: activeRequest.signal,
        });
        if (response.status === 204) continue;
        if (response.status === 401 || response.status === 403) {
          token = null;
          status.textContent = 'Authorization expired. Reopen the bootstrap URL.';
          break;
        }
        if (response.status === 503) {
          status.textContent = 'Source unavailable; waiting for a full snapshot.';
          pollCursor = null; // Repair may carry a new native generation.
          await new Promise(resolve => setTimeout(resolve, 1000));
          continue;
        }
        if (response.status !== 200) throw new Error('unexpected snapshot response');
        const announced = response.headers.get('X-Snapshot-Revision');
        const transport = transportRevision(announced);
        if (transportHighWater !== null && (transport < transportHighWater
            || (transport === transportHighWater && requestCursor !== null))) {
          throw new Error('transport revision did not advance');
        }
        // Full repair may retry an observed publication that failed to commit.
        transportHighWater = transport;
        const json = await boundedText(response);
        // Rust validates shared output before generation reset. Native revisions
        // are used only for presentation duplicate detection, never polling.
        const inspected = JSON.parse(CanvasRenderer.inspect_json(json));
        const nextGeneration = inspected.generation;
        if (generation === nextGeneration && renderer.revision() === inspected.revision) {
          pollCursor = announced;
          continue;
        }
        if (generation !== null && nextGeneration !== generation) renderer.reset();
        await renderer.present_json(json);
        pollCursor = announced;
        generation = nextGeneration;
        status.textContent = `Read-only · revision ${renderer.revision()}`;
      } catch (error) {
        if (stopped) break;
        // Do not print exceptions or request objects that might include tokens.
        status.textContent = 'Snapshot failed; reconnecting with a full snapshot.';
        pollCursor = null;
        await new Promise(resolve => setTimeout(resolve, 1000));
      } finally {
        clearTimeout(timeout);
        activeRequest = null;
      }
    }
  } finally {
    renderer.reset();
    renderer.free();
  }
}
run().catch(() => { status.textContent = 'Browser renderer could not start.'; });
