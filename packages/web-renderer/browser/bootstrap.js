import init, { CanvasRenderer } from '/bmux_web_renderer.js';

// Capability bootstrap is explicit and memory-only. Remove it before any I/O.
let token = new URLSearchParams(location.hash.slice(1)).get('token');
history.replaceState(null, '', location.pathname + location.search);
const status = document.getElementById('status');
const canvas = document.getElementById('bmux');
const container = document.getElementById('viewport');
const MAX_JSON_BYTES = 16 * 1024 * 1024;
let stopped = false;
let activeRequest;
let viewportRequest;
let controllerSecret = null;
let leaseTimer = null;
let viewportRetry = null;
let wake;
function expire() {
  stopped = true;
  token = null;
  activeRequest?.abort();
  viewportRequest?.abort();
  clearTimeout(viewportRetry);
  viewportRetry = null;
  clearTimeout(leaseTimer);
  leaseTimer = null;
  controllerSecret = null;
  wake?.();
}
addEventListener('pagehide', () => {
  if (token && controllerSecret) {
    void fetch('/view', { method: 'DELETE', keepalive: true,
      headers: { Authorization: `Bearer ${token}`, 'X-Bmux-Controller': controllerSecret },
      credentials: 'omit', redirect: 'error' }).catch(() => {});
  }
  expire();
});
function pause() {
  return new Promise(resolve => {
    const timer = setTimeout(done, 1000);
    function done() { clearTimeout(timer); wake = null; resolve(); }
    wake = done;
    if (stopped) done();
  });
}
function authorized(response) {
  if (response.status === 401 || response.status === 403) {
    expire();
    status.textContent = 'Authorization expired. Reopen the bootstrap URL.';
    throw new Error('authorization expired');
  }
  return response;
}
function request(path, controller, options = {}) {
  return fetch(path, {
    ...options,
    headers: { Authorization: `Bearer ${token}`, ...options.headers },
    cache: 'no-store', credentials: 'omit', redirect: 'error', signal: controller.signal,
  }).then(authorized);
}
async function boundedBytes(response, limit = MAX_JSON_BYTES) {
  const length = response.headers.get('Content-Length');
  if (length !== null && Number(length) > limit) throw new Error('frame too large');
  const reader = response.body.getReader();
  const chunks = [];
  let size = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.length;
      if (size > limit) throw new Error('frame too large');
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel();
    throw error;
  } finally { reader.releaseLock(); }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
  return bytes;
}
async function boundedText(response) {
  return new TextDecoder('utf-8', { fatal: true }).decode(await boundedBytes(response));
}
function transportRevision(value) {
  if (typeof value !== 'string' || !/^[1-9][0-9]{0,19}$/.test(value)) {
    throw new Error('invalid transport revision');
  }
  const parsed = BigInt(value);
  if (parsed > 18446744073709551615n) throw new Error('transport revision exceeds u64');
  return parsed;
}
function measure() {
  const style = getComputedStyle(container);
  const fontSize = Number.parseFloat(style.fontSize);
  const probe = document.createElement('canvas').getContext('2d');
  probe.font = `${style.fontSize} ${style.fontFamily}`;
  const metrics = probe.measureText('M');
  const height = Number.parseFloat(style.lineHeight);
  const ascent = metrics.fontBoundingBoxAscent ?? metrics.actualBoundingBoxAscent;
  const descent = metrics.fontBoundingBoxDescent ?? metrics.actualBoundingBoxDescent;
  const dpr = window.devicePixelRatio;
  if (![fontSize, metrics.width, height, ascent, descent, dpr].every(Number.isFinite)
      || metrics.width <= 0 || height <= 0 || dpr <= 0 || dpr > 8) {
    throw new Error('font metrics unavailable');
  }
  return { width: metrics.width, height, dpr,
    font: `${fontSize * dpr}px ${style.fontFamily}`,
    baseline: Math.max(0, Math.min(height, (height - ascent - descent) / 2 + ascent)) * dpr };
}
async function run() {
  if (!token || token.length > 4096 || /[\r\n]/.test(token)) {
    status.textContent = 'Missing or invalid bootstrap token.';
    token = null;
    return;
  }
  await init('/bmux_web_renderer_bg.wasm');
  await document.fonts.ready;
  if (stopped) return;
  const initial = measure();
  const renderer = new CanvasRenderer('bmux', initial.width * initial.dpr, initial.height * initial.dpr);
  renderer.set_metrics(initial.width * initial.dpr, initial.height * initial.dpr, initial.font, initial.baseline);
  let info = null;
  let pendingViewport = null;
  let requestId = 0n;
  let lastViewport = null;
  let layoutPending = true;
  let animation = null;
  let rendering = false;
  let dprQuery;
  function watchDpr() {
    dprQuery?.removeEventListener('change', onDpr);
    dprQuery = matchMedia(`(resolution: ${devicePixelRatio}dppx)`);
    dprQuery.addEventListener('change', onDpr, { once: true });
  }
  function onDpr() { watchDpr(); scheduleLayout(); }
  function scheduleLayout() {
    layoutPending = true;
    if (animation === null && !stopped) animation = requestAnimationFrame(() => {
      animation = null;
      if (!rendering) {
        try { applyLayout(); }
        catch { status.textContent = 'Unsupported font or Canvas allocation.'; expire(); }
      }
    });
  }
  function applyLayout() {
    if (!layoutPending || stopped) return;
    const metric = measure();
    renderer.set_metrics(metric.width * metric.dpr, metric.height * metric.dpr, metric.font, metric.baseline);
    canvas.style.width = `${canvas.width / metric.dpr}px`;
    canvas.style.height = `${canvas.height / metric.dpr}px`;
    layoutPending = false;
    if (!info?.viewport_allowed) return; // Mirror mode never sends resize.
    const cols = Math.min(65535, Math.floor(container.clientWidth / metric.width));
    const rows = Math.min(65535, Math.floor(container.clientHeight / metric.height));
    if (cols < 1 || rows < 1 || cols * rows > 262144) return;
    const key = `${info.generation}:${cols}:${rows}`;
    if (key === lastViewport) return;
    lastViewport = key;
    pendingViewport = { generation: info.generation, cols, rows };
    void sendViewport();
  }
  async function sendViewport() {
    if (viewportRequest || viewportRetry !== null || stopped || !pendingViewport) return;
    const value = pendingViewport;
    pendingViewport = null;
    if (!info?.viewport_allowed || value.generation !== info.generation) return;
    if (requestId === 18446744073709551615n) { expire(); return; }
    const secret = controllerSecret;
    viewportRequest = new AbortController();
    const controller = viewportRequest;
    const timeout = setTimeout(() => controller.abort(), 5000);
    try {
      const response = await request('/viewport', controller, { method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-Bmux-Controller': secret },
        body: JSON.stringify({ ...value, request_id: (++requestId).toString() }) });
      if (stopped || secret !== controllerSecret || value.generation !== info?.generation) return;
      if (response.status === 429) {
        pendingViewport ??= value;
        viewportRetry = setTimeout(() => { viewportRetry = null; void sendViewport(); }, 100);
      } else if (response.status === 409) { info = null; pendingViewport = null; lastViewport = null; }
      else if (response.status !== 202) throw new Error('viewport rejected');
    } catch {
      if (!stopped && secret === controllerSecret && value.generation === info?.generation) {
        info = null; pendingViewport = null; lastViewport = null;
      }
    } finally {
      clearTimeout(timeout);
      viewportRequest = null;
      if (pendingViewport && !stopped) void sendViewport();
    }
  }
  let claimRequest = null;
  async function renewLease() {
    leaseTimer = null;
    if (stopped || !controllerSecret) return;
    if (rendering) { leaseTimer = setTimeout(renewLease, 1000); return; }
    const renewal = new AbortController();
    const timeout = setTimeout(() => renewal.abort(), 5000);
    try { await refreshInfo(renewal); }
    catch { if (!stopped) { info = null; pendingViewport = null; } }
    finally { clearTimeout(timeout); }
  }
  async function refreshInfo(controller) {
    if (claimRequest) return claimRequest;
    claimRequest = claim(controller);
    try { await claimRequest; } finally { claimRequest = null; }
  }
  async function claim(controller) {
    clearTimeout(leaseTimer);
    leaseTimer = null;
    const response = await request('/view', controller, { method: 'POST',
      headers: controllerSecret ? { 'X-Bmux-Controller': controllerSecret } : {} });
    if (response.status !== 200) throw new Error('view claim unavailable');
    const next = JSON.parse(await boundedText(response));
    if (stopped) return;
    if (!Array.isArray(next.presentation_versions) || !next.presentation_versions.includes(1)) {
      expire();
      status.textContent = 'Incompatible presentation protocol. Rebuild browser assets and native BMUX together.';
      throw new Error('unsupported presentation protocol');
    }
    if (next.protocol_version !== 1 || (next.generation !== null && (typeof next.generation !== 'string'
        || next.generation.length < 1 || next.generation.length > 128))
        || typeof next.viewport_allowed !== 'boolean'
        || (next.viewport_allowed && (typeof next.controller !== 'string'
          || next.controller.length < 1 || next.controller.length > 4096
          || /[\r\n]/.test(next.controller) || next.generation === null || next.lease_ms < 1000))
        || !Number.isFinite(next.lease_ms) || next.lease_ms < 0) {
      throw new Error('invalid view claim');
    }
    // Expired owners may reclaim once without their stale secret. Mirrors do
    // not repeatedly claim ownership while they observe a healthy generation.
    if (!next.viewport_allowed && controllerSecret !== null) {
      controllerSecret = null;
      requestId = 0n;
      return claim(controller);
    }
    if (controllerSecret !== next.controller) requestId = 0n;
    controllerSecret = next.viewport_allowed ? next.controller : null;
    info = next;
    container.style.overflow = next.viewport_allowed ? 'hidden' : 'auto';
    pendingViewport = null;
    lastViewport = null;
    scheduleLayout();
    if (controllerSecret) leaseTimer = setTimeout(renewLease,
      Math.max(1000, Math.min(10000, next.lease_ms / 3)));
  }
  const observer = new ResizeObserver(scheduleLayout);
  observer.observe(container);
  document.fonts.addEventListener('loadingdone', scheduleLayout);
  watchDpr();
  // Transport publications and native presentation revisions are independent.
  let pollCursor = null;
  let transportHighWater = null;
  let generation = null;
  try {
    while (!stopped) {
      activeRequest = new AbortController();
      const controller = activeRequest;
      const timeout = setTimeout(() => controller.abort(), 35000);
      try {
        if (info === null) await refreshInfo(controller);
        const requestCursor = pollCursor;
        const path = requestCursor === null ? '/presentation?version=1'
          : `/presentation?version=1&after=${encodeURIComponent(requestCursor)}&base=${encodeURIComponent(requestCursor)}`;
        const response = await request(path, controller);
        if (response.status === 204) continue;
        if (response.status === 503) {
          status.textContent = 'Source unavailable; waiting for a full snapshot.';
          pollCursor = null;
          info = null;
          await pause();
          continue;
        }
        if (response.status !== 200) throw new Error('unexpected snapshot response');
        const announced = response.headers.get('X-Snapshot-Revision');
        const transport = transportRevision(announced);
        if (transportHighWater !== null && (transport < transportHighWater
            || (transport === transportHighWater && requestCursor !== null))) {
          throw new Error('transport revision did not advance');
        }
        transportHighWater = transport;
        if (response.headers.get('Content-Type') !== 'application/vnd.bmux.presentation-v1') {
          throw new Error('unexpected presentation representation');
        }
        const bytes = await boundedBytes(response);
        renderer.discard_binary_pending();
        const requirements = JSON.parse(CanvasRenderer.binary_resources(bytes));
        for (const resource of requirements) {
          if (renderer.has_binary_resource(resource.id)) continue;
          const loaded = await request(`/resource/${encodeURIComponent(resource.id)}`, controller);
          if (loaded.status !== 200 || loaded.headers.get('Content-Type') !== resource.mime_type) {
            throw new Error('image resource unavailable');
          }
          renderer.install_binary_resource(resource.id, await boundedBytes(loaded, 8 * 1024 * 1024));
        }
        const inspected = JSON.parse(renderer.inspect_binary(bytes));
        if (!inspected.supported) {
          status.textContent = 'Snapshot requires an unsupported Canvas feature.';
          expire();
          break;
        }
        const nextGeneration = inspected.generation;
        if (nextGeneration !== generation) await refreshInfo(controller);
        if (claimRequest) await claimRequest;
        if (stopped || (info?.viewport_allowed && nextGeneration !== info.generation)) {
          throw new Error('obsolete generation');
        }
        if (generation === nextGeneration && renderer.revision() === inspected.revision) {
          pollCursor = announced;
          continue;
        }
        rendering = true;
        try {
          await renderer.present_binary(bytes);
        }
        finally { rendering = false; }
        if (stopped) break;
        pollCursor = announced;
        generation = nextGeneration;
        scheduleLayout();
        const unavailable = inspected.unsupported_metadata.length
          ? ` · Unavailable metadata: ${inspected.unsupported_metadata.join(', ')}` : '';
        const partial = inspected.partial_metadata.length
          ? ` · Partial metadata: ${inspected.partial_metadata.join(', ')}` : '';
        const sourceText = inspected.selection_text_available ? '' : ' · Selection source text unavailable';
        status.textContent = `Read-only · revision ${renderer.revision()}${unavailable}${partial}${sourceText}`;
      } catch {
        renderer.discard_binary_pending();
        if (stopped) break;
        status.textContent = 'Snapshot failed; reconnecting with a full snapshot.';
        pollCursor = null;
        info = null;
        await pause();
      } finally {
        clearTimeout(timeout);
        activeRequest = null;
      }
    }
  } finally {
    expire();
    observer.disconnect();
    document.fonts.removeEventListener('loadingdone', scheduleLayout);
    dprQuery?.removeEventListener('change', onDpr);
    if (animation !== null) cancelAnimationFrame(animation);
    try { renderer.reset(); } finally { renderer.free(); }
  }
}
run().catch(() => { if (!stopped) status.textContent = 'Browser renderer could not start.'; });
