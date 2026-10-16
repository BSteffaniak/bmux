import { mountCanvasViewer } from './canvas-viewer.js';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export function revision(value) {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,19})$/.test(value)
      || BigInt(value) > 18446744073709551615n) throw new Error('invalid cursor');
  return value;
}
export function validateStatus(value, now = Date.now()) {
  if (value.protocol !== 1 || value.renderer !== 'bmwp-3') throw new Error('incompatible broadcast');
  if (!['offline', 'starting', 'live', 'paused'].includes(value.state)) throw new Error('invalid state');
  revision(value.revision);
  if (value.epoch !== null && (typeof value.epoch !== 'string' || !UUID.test(value.epoch))) throw new Error('invalid epoch');
  if (value.state !== 'offline' && (!value.epoch || !Number.isSafeInteger(value.lease_expires_at))) {
    throw new Error('invalid lease');
  }
  if (value.state !== 'offline' && value.lease_expires_at <= now) return { ...value, state: 'offline' };
  if (value.state === 'live' && value.revision === '0') throw new Error('missing frame');
  return value;
}
async function boundedBytes(response, maximum) {
  const announced = response.headers.get('Content-Length');
  if (announced && (!/^[0-9]+$/.test(announced) || Number(announced) > maximum)) {
    await response.body?.cancel();
    throw new Error('oversize response');
  }
  const reader = response.body?.getReader();
  if (!reader) throw new Error('missing body');
  const bytes = new Uint8Array(maximum);
  let size = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (size + value.byteLength > maximum) throw new Error('oversize response');
      bytes.set(value, size);
      size += value.byteLength;
    }
  } finally { await reader.cancel(); reader.releaseLock(); }
  return bytes.subarray(0, size);
}

// Explicit DOM, same-origin API and asset locations; no local capability bootstrap.
export async function mountLiveViewer({ container, status, apiBase, assetBase,
  fetchImpl = globalThis.fetch.bind(globalThis), canvasFactory = mountCanvasViewer,
  autoStart = true, now = Date.now }) {
  const base = new URL(apiBase, container.ownerDocument.baseURI);
  if (base.origin !== new URL(container.ownerDocument.baseURI).origin || base.search || base.hash) {
    throw new Error('broadcast API must be same-origin');
  }
  if (!base.pathname.endsWith('/')) base.pathname += '/';
  const viewer = await canvasFactory({ container, assetBase });
  let disposed = false;
  let state = null;
  let ticket = 0;
  let cursor = null;
  let frameIdentity = null;
  const deltaCapable = viewer.capabilities?.includes('bmwp-delta-v1') === true;
  const counters = { wire_bytes: 0, full: 0, delta: 0, repair: 0 };
  const count = (key, amount = 1) => { counters[key] = Math.min(Number.MAX_SAFE_INTEGER, counters[key] + amount); };
  let statusBusy = false;
  let frameBusy = false;
  let statusTimer;
  let frameTimer;
  let leaseTimer;
  const requests = new Set();
  let frameController;
  function hide(label) {
    ticket++;
    state = null;
    cursor = null;
    frameIdentity = null;
    frameController?.abort();
    clearTimeout(leaseTimer);
    viewer.clear();
    status.textContent = label;
  }
  async function request(path, consume, isFrame = false) {
    const controller = new AbortController();
    if (isFrame) frameController = controller;
    requests.add(controller);
    const timeout = setTimeout(() => controller.abort(), 5000);
    try {
      const response = await fetchImpl(new URL(path, base).href, {
        ...(isFrame && deltaCapable ? { headers: { 'X-Bmux-Capabilities': 'bmwp-delta-v1',
          ...(typeof DecompressionStream === 'function' ? { 'X-Bmux-Compression': 'gzip-v1' } : {}) } } : {}),
        signal: controller.signal, credentials: 'omit', redirect: 'error', cache: 'no-store',
      });
      return await consume(response);
    } finally {
      clearTimeout(timeout);
      requests.delete(controller);
      if (frameController === controller) frameController = null;
    }
  }
  async function pollStatus() {
    if (disposed || statusBusy) return;
    statusBusy = true;
    try {
      const next = await request('status', async response => {
        if (response.status !== 200 || !response.headers.get('Content-Type')?.startsWith('application/json')) {
          throw new Error('status unavailable');
        }
        return validateStatus(JSON.parse(new TextDecoder().decode(await boundedBytes(response, 16 * 1024))), now());
      });
      if (disposed) return;
      if (state?.epoch === next.epoch && BigInt(next.revision) < BigInt(state.revision)) {
        throw new Error('regressing cursor');
      }
      // Revisions are content watermarks, not authorization transitions.
      // A busy producer can advance between every status and frame request.
      // Keep retained output and in-flight decoding within the same live epoch;
      // current() still rejects frames behind the latest observed watermark.
      if (!state || next.epoch !== state.epoch || next.state !== state.state) {
        hide(next.state === 'live' ? 'Loading live output…' : `Broadcast ${next.state}`);
      }
      state = next;
      clearTimeout(leaseTimer);
      if (next.state !== 'offline') leaseTimer = setTimeout(() => hide('Broadcast offline'), Math.max(0, next.lease_expires_at - now()));
    } catch {
      if (!disposed) hide('Broadcast unavailable or incompatible');
    } finally {
      statusBusy = false;
      if (autoStart && !disposed) statusTimer = setTimeout(pollStatus, 1000);
    }
  }
  async function pollFrame() {
    if (disposed || frameBusy || state?.state !== 'live') return;
    const admitted = state;
    const admittedTicket = ticket;
    let deliveredRevision = admitted.revision;
    const current = () => !disposed && ticket === admittedTicket && state?.state === 'live'
      && state.epoch === admitted.epoch && BigInt(state.revision) <= BigInt(deliveredRevision)
      && state.lease_expires_at > now();
    frameBusy = true;
    try {
      const path = `presentation?epoch=${encodeURIComponent(admitted.epoch)}${cursor === null ? '' : `&after=${cursor}`}`
        + (deltaCapable && frameIdentity ? `&generation=${encodeURIComponent(frameIdentity.generation)}&base=${frameIdentity.revision}` : '');
      let deliveredIdentity = null;
      let isDelta = false;
      const bytes = await request(path, async response => {
        if (response.status === 204) { await response.body?.cancel(); return null; }
        const receivedRevision = revision(response.headers.get('X-Snapshot-Revision'));
        if (response.status !== 200 || response.headers.get('Content-Type')?.split(';')[0] !== 'application/octet-stream'
            || response.headers.get('X-Bmux-Epoch') !== admitted.epoch
            || BigInt(receivedRevision) < BigInt(admitted.revision)
            || (cursor !== null && BigInt(receivedRevision) < BigInt(cursor))) {
          await response.body?.cancel();
          throw new Error('presentation not admitted');
        }
        deliveredRevision = receivedRevision;
        const negotiated = response.headers.get('X-Bmux-Capabilities');
        if (negotiated && negotiated !== 'bmwp-delta-v1') throw new Error('unsupported capability');
        if (negotiated === 'bmwp-delta-v1') {
          if (!deltaCapable) throw new Error('unnegotiated delta');
          const kind = response.headers.get('X-Bmux-Frame');
          const encodedGeneration = response.headers.get('X-Bmux-Generation');
          const generation = encodedGeneration === null ? null : decodeURIComponent(encodedGeneration);
          if (!['full', 'delta'].includes(kind) || !generation || generation.length > 128
              || /[\u0000-\u001f\u007f]/.test(generation)) throw new Error('invalid frame identity');
          deliveredIdentity = { generation, revision: revision(response.headers.get('X-Bmux-Frame-Revision')) };
          isDelta = kind === 'delta';
          if (isDelta && (!frameIdentity || frameIdentity.generation !== generation
              || response.headers.get('X-Bmux-Base-Revision') !== cursor
              || response.headers.get('X-Bmux-Base-Frame-Revision') !== frameIdentity.revision
              || BigInt(deliveredIdentity.revision) <= BigInt(frameIdentity.revision))) throw new Error('missing delta base');
        } else if (response.headers.get('X-Bmux-Frame') === 'delta') throw new Error('unnegotiated delta');
        const wire = await boundedBytes(response, 8 * 1024 * 1024);
        count('wire_bytes', wire.length);
        const compression = response.headers.get('X-Bmux-Compression');
        if (!compression) return wire;
        if (compression !== 'gzip-v1' || !deltaCapable || typeof DecompressionStream !== 'function') throw new Error('unnegotiated compression');
        const decoded = new Blob([wire]).stream().pipeThrough(new DecompressionStream('gzip'));
        return boundedBytes(new Response(decoded), 8 * 1024 * 1024);
      }, true);
      if (bytes === null || !current()) return;
      if (bytes.length < 5 || bytes[0] !== 66 || bytes[1] !== 77 || bytes[2] !== 87 || bytes[3] !== 80 || bytes[4] !== 3) {
        throw new Error('incompatible presentation');
      }
      const presented = await viewer.present(bytes, current);
      if (!current()) {
        // A stateful decoder may have rejected this unadmitted update and
        // dropped its base. Never advertise the older cursor after that race.
        cursor = null; frameIdentity = null; viewer.clear();
        return;
      }
      if (presented) {
        cursor = deliveredRevision;
        frameIdentity = deliveredIdentity;
        count(isDelta ? 'delta' : 'full');
        status.textContent = 'Live · read-only';
      } else if (current()) {
        count('repair');
        cursor = null; frameIdentity = null; viewer.clear();
      }
    } catch {
      if (current()) { count('repair'); hide('Broadcast unavailable or incompatible'); }
    } finally { frameBusy = false; }
  }
  async function frameLoop() {
    await pollFrame();
    if (!disposed) frameTimer = setTimeout(frameLoop, 1000);
  }
  const handle = {
    pollStatus, pollFrame,
    get counters() { return { ...counters }; },
    dispose() {
      disposed = true;
      hide('Broadcast offline');
      clearTimeout(statusTimer); clearTimeout(frameTimer);
      for (const controller of requests) controller.abort();
      viewer.dispose();
    },
  };
  status.textContent = 'Connecting to broadcast…';
  if (autoStart) { void pollStatus(); void frameLoop(); }
  return handle;
}
