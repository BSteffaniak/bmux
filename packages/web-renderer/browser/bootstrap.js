// Capability bootstrap is explicit and memory-only. Remove it before any I/O.
let token = new URLSearchParams(location.hash.slice(1)).get('token');
history.replaceState(null, '', location.pathname + location.search);
const status = document.getElementById('status');
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
  if (!stopped && controllerSecret) {
    void fetch('/view', { method: 'DELETE', keepalive: true,
      headers: { ...(token ? { Authorization: `Bearer ${token}` } : {}), 'X-Bmux-Controller': controllerSecret },
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
    status.textContent = 'Access denied. Token-protected views require the printed bootstrap URL.';
    throw new Error('authorization expired');
  }
  return response;
}
let diagnosticSequence = 0;
const diagnosticEvents = [];
function diagnostic(code, details = {}) {
  // Only fixed fields and route classes: never URLs, tokens, bodies or error text.
  const event = { sequence: ++diagnosticSequence, elapsed_ms: Math.round(performance.now()), code, ...details };
  if (diagnosticEvents.length === 64) diagnosticEvents.shift();
  diagnosticEvents.push(event);
  if (diagnosticEvents.length === 1 || code === 'recovered') console.info('bmux browser', event);
}
window.__bmuxDiagnostics = () => ({ version: 1, events: diagnosticEvents.map(event => ({ ...event })) });
function routeClass(path) {
  const route = path.split('?')[0];
  return route.startsWith('/resource/') ? 'resource' :
    ['/document', '/presentation', '/snapshot', '/view', '/viewport', '/info', '/diagnostics'].includes(route) ? route.slice(1) : 'other';
}
function request(path, controller, options = {}) {
  const route = routeClass(path);
  return fetch(path, {
    ...options,
    headers: { ...(token ? { Authorization: `Bearer ${token}` } : {}), ...options.headers },
    cache: 'no-store', credentials: 'omit', redirect: 'error', signal: controller.signal,
  }).then(response => {
    if (response.status >= 400) diagnostic('http_rejected', { route, status: response.status });
    return authorized(response);
  }, error => {
    const code = controller.signal.aborted ? 'request_aborted' : 'network_unavailable';
    diagnostic(code, { route });
    const classified = new Error(`${route}: ${code === 'request_aborted' ? 'request cancelled or deadline exceeded' : 'HTTP host unreachable or connection rejected (no HTTP response)'}`);
    classified.bmuxCode = code;
    throw classified;
  });
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
async function run() {
  // Canvas measurement and WASM are exclusive to fixed-frame delivery.
  function measure() {
    const style = getComputedStyle(container);
    const fontSize = Number.parseFloat(style.fontSize);
    const probe = document.createElement('canvas').getContext('2d');
    probe.font = `${style.fontSize} ${style.fontFamily}`;
    const metrics = probe.measureText('M');
    const height = Number.parseFloat(style.lineHeight);
    const ascent = metrics.fontBoundingBoxAscent ?? metrics.actualBoundingBoxAscent;
    const descent = metrics.fontBoundingBoxDescent ?? metrics.actualBoundingBoxDescent;
    const requestedDpr = window.devicePixelRatio;
    if (![fontSize, metrics.width, height, ascent, descent, requestedDpr].every(Number.isFinite)
        || metrics.width <= 0 || height <= 0 || requestedDpr <= 0) {
      throw new Error('font metrics unavailable');
    }
    // Fixed-frame raster metrics obey the native per-cell limit. CSS cell
    // metrics (and therefore leased producer resize requests) stay unchanged.
    const dpr = Math.min(requestedDpr, 4, 256 / metrics.width, 256 / height);
    return { width: metrics.width, height, dpr,
      font: `${fontSize * dpr}px ${style.fontFamily}`,
      baseline: Math.max(0, Math.min(height, (height - ascent - descent) / 2 + ascent)) * dpr };
  }
  if (token !== null && (!token || token.length > 4096 || /[\r\n]/.test(token))) {
    status.textContent = 'Missing or invalid bootstrap token.';
    token = null;
    return;
  }
  const { default: init, CanvasRenderer } = await import('/bmux_web_renderer.js');
  await init('/bmux_web_renderer_bg.wasm');
  await document.fonts.ready;
  if (stopped) return;
  const canvas = document.createElement('canvas');
  canvas.id = 'bmux';
  canvas.setAttribute('aria-label', 'Read-only terminal presentation');
  container.append(canvas);
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
        catch { status.textContent = 'Local layout failed; last complete frame retained.'; }
      }
    });
  }
  function applyLayout() {
    if (!layoutPending || stopped) return;
    const metric = measure();
    renderer.set_metrics(metric.width * metric.dpr, metric.height * metric.dpr, metric.font, metric.baseline);
    const scale = Math.min(container.clientWidth / canvas.width, container.clientHeight / canvas.height);
    canvas.style.width = `${canvas.width * scale}px`;
    canvas.style.height = `${canvas.height * scale}px`;
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
    if (next.presentation_status) {
      status.textContent = `Presentation paused · ${next.presentation_status} · showing last complete frame`;
    }
    info = next;
    container.style.overflow = 'hidden';
    pendingViewport = null;
    lastViewport = null;
    scheduleLayout();
    if (controllerSecret) leaseTimer = setTimeout(renewLease,
      Math.max(1000, Math.min(10000, next.lease_ms / 3)));
  }
  const observer = new ResizeObserver(scheduleLayout);
  observer.observe(container);
  addEventListener('resize', scheduleLayout);
  window.visualViewport?.addEventListener('resize', scheduleLayout);
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
        if (response.status === 204) {
          // Publication rejection does not advance snapshots; refresh its status
          // on a bounded long-poll wake while keeping the committed Canvas intact.
          await claim(controller);
          continue;
        }
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
        finally {
          rendering = false;
          if (layoutPending) scheduleLayout();
        }
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
    removeEventListener('resize', scheduleLayout);
    window.visualViewport?.removeEventListener('resize', scheduleLayout);
    document.fonts.removeEventListener('loadingdone', scheduleLayout);
    dprQuery?.removeEventListener('change', onDpr);
    if (animation !== null) cancelAnimationFrame(animation);
    try { renderer.reset(); } finally { renderer.free(); }
  }
}
// Generic full-document delivery. No product model or viewport mutation transport.
function validateDocument(value, announced) {
  const identity = value => typeof value === 'string' && /^[A-Za-z0-9._:-]{1,128}$/.test(value);
  if (!value || value.version !== 1 || value.publication_revision !== announced ||
      !identity(value.generation) || !identity(value.capability)) throw new Error('invalid document identity');
  transportRevision(value.publication_revision);
  transportRevision(value.revision);
  if (!Array.isArray(value.content) || !value.content.length || value.content.length > MAX_JSON_BYTES / 4 ||
      !value.content.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255) ||
      !Array.isArray(value.resources) || value.resources.length > 1024) throw new Error('invalid document bounds');
  let total = 0;
  const ids = new Set();
  for (const resource of value.resources) {
    if (!resource || typeof resource.id !== 'string' || !/^[a-f0-9]{64}$/.test(resource.id) || ids.has(resource.id) ||
        typeof resource.mime_type !== 'string' || resource.mime_type.length > 128 ||
        !/^[\x20-\x7e]+$/.test(resource.mime_type) || !resource.mime_type.includes('/') ||
        !Number.isSafeInteger(resource.length) || resource.length < 0 || resource.length > 8 * 1024 * 1024) {
      throw new Error('invalid document resource');
    }
    ids.add(resource.id);
    total += resource.length;
    if (total > 32 * 1024 * 1024) throw new Error('document resource budget exceeded');
    Object.freeze(resource);
  }
  Object.freeze(value.content);
  Object.freeze(value.resources);
  return Object.freeze(value);
}

// Resource integrity is required even on HTTP LAN origins, where SubtleCrypto
// is unavailable. This does not authenticate an unencrypted HTTP transport.
async function resourceDigest(bytes) {
  if (typeof globalThis.crypto?.subtle?.digest === 'function') {
    return new Uint8Array(await globalThis.crypto.subtle.digest('SHA-256', bytes));
  }
  if (typeof Uint32Array !== 'function' || typeof DataView !== 'function') {
    throw new Error('Browser cannot verify document resources: SHA-256 requires Web Crypto or typed-array support. Use a modern browser or trusted HTTPS.');
  }
  if (bytes.length > 8 * 1024 * 1024) throw new Error('resource verification byte budget exceeded');
  // FIPS 180-4 SHA-256. Resource bounds are enforced before allocating padding.
  const constants = new Uint32Array([
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
  ]);
  const hash = new Uint32Array([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
  ]);
  const padded = new Uint8Array(Math.ceil((bytes.length + 9) / 64) * 64);
  padded.set(bytes);
  padded[bytes.length] = 0x80;
  const view = new DataView(padded.buffer);
  const bits = bytes.length * 8;
  view.setUint32(padded.length - 8, Math.floor(bits / 0x100000000));
  view.setUint32(padded.length - 4, bits >>> 0);
  const words = new Uint32Array(64);
  const rotate = (value, count) => (value >>> count) | (value << (32 - count));
  for (let offset = 0; offset < padded.length; offset += 64) {
    // Keep large (up to 8 MiB) resources from monopolizing the browser thread.
    if (offset !== 0 && offset % 65536 === 0) {
      await new Promise(resolve => setTimeout(resolve, 0));
      if (stopped) throw new Error('resource verification cancelled');
    }
    for (let i = 0; i < 16; i++) words[i] = view.getUint32(offset + i * 4);
    for (let i = 16; i < 64; i++) {
      const x = words[i - 15], y = words[i - 2];
      const s0 = rotate(x, 7) ^ rotate(x, 18) ^ (x >>> 3);
      const s1 = rotate(y, 17) ^ rotate(y, 19) ^ (y >>> 10);
      words[i] = words[i - 16] + s0 + words[i - 7] + s1;
    }
    let [a, b, c, d, e, f, g, h] = hash;
    for (let i = 0; i < 64; i++) {
      const s1 = rotate(e, 6) ^ rotate(e, 11) ^ rotate(e, 25);
      const choice = (e & f) ^ (~e & g);
      const t1 = (h + s1 + choice + constants[i] + words[i]) >>> 0;
      const s0 = rotate(a, 2) ^ rotate(a, 13) ^ rotate(a, 22);
      const majority = (a & b) ^ (a & c) ^ (b & c);
      h = g; g = f; f = e; e = (d + t1) >>> 0;
      d = c; c = b; b = a; a = (t1 + s0 + majority) >>> 0;
    }
    const next = [a, b, c, d, e, f, g, h];
    for (let i = 0; i < 8; i++) hash[i] += next[i];
  }
  const digest = new Uint8Array(32);
  const output = new DataView(digest.buffer);
  for (let i = 0; i < 8; i++) output.setUint32(i * 4, hash[i]);
  return digest;
}

function documentAdapterPath(value) {
  // Assets are explicit same-origin modules, not arbitrary URLs or product dispatch.
  if (typeof value !== 'string' || !/^\/[A-Za-z0-9_/-]+\.js$/.test(value) ||
      value.startsWith('//') || value.includes('/../') || value.includes('/./')) {
    throw new Error('document delivery requires a same-origin adapter module');
  }
  return value;
}

async function runDocument(adapterPath) {
  if (token !== null && (!token || token.length > 4096 || /[\r\n]/.test(token))) {
    expire();
    throw new Error('invalid bootstrap token');
  }
  const module = await import(documentAdapterPath(adapterPath));
  const adapter = await module.createDocumentRenderer({ mount: container });
  if (!adapter || typeof adapter.capability !== 'string' || typeof adapter.prepare !== 'function' ||
      typeof adapter.reset !== 'function' || typeof adapter.free !== 'function') {
    throw new Error('invalid document adapter');
  }
  let committed = null;
  let committedViewport = null;
  const inspection = () => committed ? {
    generation: committed.generation, revision: committed.revision,
    publication_revision: committed.publication_revision,
    viewport: committedViewport,
    frame: adapter.inspectCommittedFrame?.() ?? null,
  } : null;
  if (new URLSearchParams(location.search).get('inspect') === '1') {
    window.__bmuxDocumentInspection = inspection;
  }
  let drawTask = null;
  let layoutEpoch = 0;
  let layoutPending = false;
  let animation = null;
  let dprQuery;
  let cursor = null;
  let highWater = null;
  let consecutiveFailures = 0;
  let lastSuccess = null;
  const cache = new Map();
  let cacheBytes = 0;
  let resourceRequests = 0;
  const lifetime = new AbortController();
  const stop = () => lifetime.abort();
  addEventListener('pagehide', stop);
  function viewport() {
    const bounds = container.getBoundingClientRect();
    // CSS geometry is not a raster allocation. The adapter bounds raster density.
    const dimension = value => Number.isFinite(value) ? Math.max(1, value) : 1;
    const density = window.devicePixelRatio;
    return Object.freeze({ width: dimension(bounds.width), height: dimension(bounds.height),
      device_pixel_ratio: Number.isFinite(density) && density > 0 ? density : 1 });
  }
  async function resourceFor(document, id) {
    const descriptor = document.resources.find(resource => resource.id === id);
    if (!descriptor) throw new Error('resource absent from document manifest');
    const retained = cache.get(id);
    if (retained) {
      if (retained.mime !== descriptor.mime_type || retained.bytes.length !== descriptor.length) {
        throw new Error('resource identity changed');
      }
      return retained.bytes.slice();
    }
    if (resourceRequests >= 8) throw new Error('document resource concurrency budget exceeded');
    resourceRequests++;
    const controller = new AbortController();
    const cancel = () => controller.abort();
    lifetime.signal.addEventListener('abort', cancel, { once: true });
    if (lifetime.signal.aborted) controller.abort();
    const timer = setTimeout(cancel, 10000);
    try {
      const response = await request(`/resource/${id}`, controller);
      if (response.status !== 200 || response.headers.get('Content-Type') !== descriptor.mime_type) {
        throw new Error('document resource unavailable');
      }
      const bytes = await boundedBytes(response, 8 * 1024 * 1024);
      if (bytes.length !== descriptor.length) throw new Error('document resource length mismatch');
      const digest = await resourceDigest(bytes);
      const hash = Array.from(digest, byte => byte.toString(16).padStart(2, '0')).join('');
      if (hash !== id) throw new Error('document resource digest mismatch');
      while (cacheBytes + bytes.length > 32 * 1024 * 1024 || cache.size >= 1024) {
        const oldest = cache.keys().next().value;
        if (oldest === undefined) throw new Error('resource cache exhausted');
        cacheBytes -= cache.get(oldest).bytes.length;
        cache.delete(oldest);
      }
      cache.set(id, { mime: descriptor.mime_type, bytes });
      cacheBytes += bytes.length;
      return bytes.slice();
    } finally {
      resourceRequests--;
      clearTimeout(timer);
      lifetime.signal.removeEventListener('abort', cancel);
    }
  }
  function committedStatus(revision) {
    let frame;
    try { frame = adapter.inspectCommittedFrame?.(); }
    catch { return `Read-only · responsive · revision ${revision} · Committed metadata inspection unavailable`; }
    const kinds = values => Array.isArray(values) && values.length <= 16 &&
      values.every(kind => typeof kind === 'string' && /^[a-z_]{1,32}$/.test(kind)) ? values : [];
    const unavailable = kinds(frame?.unsupported_metadata);
    const partial = kinds(frame?.partial_metadata);
    return `Read-only · responsive · revision ${revision}` +
      (unavailable.length ? ` · Unavailable metadata: ${unavailable.join(', ')}` : '') +
      (partial.length ? ` · Partial metadata: ${partial.join(', ')}` : '') +
      (frame?.selection_text_available === false ? ' · Selection source text unavailable' : '') +
      (frame == null ? ' · Committed metadata inspection unavailable' : '');
  }
  async function draw(document) {
    if (document.capability !== adapter.capability) throw new Error('unsupported document capability');
    if (committed && committed.generation === document.generation &&
        (BigInt(document.revision) < BigInt(committed.revision) ||
         (document.revision === committed.revision && document.publication_revision !== committed.publication_revision))) {
      throw new Error('document revision did not advance');
    }
    for (let attempts = 0; attempts < 8; attempts++) {
      const epoch = layoutEpoch;
      layoutPending = false;
      const nextViewport = viewport();
      let transaction = null;
      try {
        transaction = await adapter.prepare(document, nextViewport, {
          requestResource: id => resourceFor(document, id), resourceDigest, signal: lifetime.signal,
        });
        if (!transaction || typeof transaction.commit !== 'function' || typeof transaction.discard !== 'function') {
          throw new Error('invalid document transaction');
        }
        if (stopped || lifetime.signal.aborted) return;
        const latestViewport = viewport();
        if (epoch !== layoutEpoch || nextViewport.width !== latestViewport.width ||
            nextViewport.height !== latestViewport.height ||
            nextViewport.device_pixel_ratio !== latestViewport.device_pixel_ratio) continue;
        // Adapter commit must be synchronous and all-or-nothing, including metadata.
        transaction.commit();
        transaction = null;
        committed = document;
        committedViewport = nextViewport;
        return;
      } finally { transaction?.discard(); }
    }
    throw new Error('local layout changed too frequently');
  }
  async function apply(document) {
    // Re-check after awaiting: a queued publication must not race a local reflow.
    while (drawTask) await drawTask.catch(() => {});
    if (stopped) return;
    const task = draw(document);
    drawTask = task;
    try { await task; } finally {
      if (drawTask === task) drawTask = null;
      if (layoutPending) scheduleReflow();
    }
  }
  function scheduleReflow() {
    if (animation !== null || stopped) return;
    animation = requestAnimationFrame(() => {
      animation = null;
      if (!layoutPending || !committed || drawTask || stopped) return;
      void apply(committed).then(() => {
        // Local layout is not evidence that the document transport recovered.
        if (!stopped && consecutiveFailures === 0) status.textContent = committedStatus(committed.revision);
      }).catch(() => {
        if (!stopped && consecutiveFailures === 0) status.textContent = 'Local reflow failed; last committed output retained.';
      });
    });
  }
  function reflow() {
    // Invalidate immediately, even when a prepare is in flight. Its finally block
    // drains the latest pending layout after success OR failure.
    layoutEpoch++;
    layoutPending = true;
    scheduleReflow();
  }
  function watchDpr() {
    dprQuery?.removeEventListener('change', onDpr);
    dprQuery = matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    dprQuery.addEventListener('change', onDpr, { once: true });
  }
  function onDpr() { watchDpr(); reflow(); }
  const observer = new ResizeObserver(reflow);
  observer.observe(container);
  addEventListener('resize', reflow);
  document.fonts.addEventListener('loadingdone', reflow);
  window.visualViewport?.addEventListener('resize', reflow);
  watchDpr();
  void document.fonts.ready.then(() => { if (!stopped) reflow(); });
  try {
    while (!stopped) {
      const controller = new AbortController();
      activeRequest = controller;
      const timer = setTimeout(() => controller.abort(), 35000);
      try {
        const response = await request(`/document?version=1${cursor === null ? '' : `&after=${cursor}`}`, controller);
        if (stopped) break;
        if (response.status === 204) continue;
        if (response.status !== 200 || response.headers.get('Content-Type') !== 'application/vnd.bmux.document-v1+json') {
          throw new Error('document unavailable');
        }
        const announced = response.headers.get('X-Snapshot-Revision');
        const transport = transportRevision(announced);
        if (highWater !== null && (transport < highWater || (transport === highWater && cursor !== null))) {
          throw new Error('obsolete document publication');
        }
        const next = validateDocument(JSON.parse(await boundedText(response)), announced);
        if (stopped) break;
        await apply(next);
        if (stopped) break;
        if (consecutiveFailures) diagnostic('recovered', { failures: consecutiveFailures });
        consecutiveFailures = 0;
        status.textContent = committedStatus(committed.revision);
        lastSuccess = performance.now();
        highWater = transport;
        cursor = announced;
      } catch (error) {
        if (stopped) break;
        consecutiveFailures++;
        diagnostic('document_repair', { failures: consecutiveFailures,
          reason: error.bmuxCode || 'document_or_render_rejected',
          retained: committed !== null });
        const age = lastSuccess === null ? 'none yet' : `${Math.floor((performance.now() - lastSuccess) / 1000)}s ago`;
        status.textContent = `Document unavailable; last committed output retained. Full-document retry ${consecutiveFailures}; last success ${age}. ${error.message}. Diagnostics: window.__bmuxDiagnostics()`;
        cursor = null;
        await pause();
      } finally { clearTimeout(timer); activeRequest = null; }
    }
  } finally {
    lifetime.abort();
    observer.disconnect();
    removeEventListener('resize', reflow);
    removeEventListener('pagehide', stop);
    document.fonts.removeEventListener('loadingdone', reflow);
    window.visualViewport?.removeEventListener('resize', reflow);
    dprQuery?.removeEventListener('change', onDpr);
    if (animation !== null) cancelAnimationFrame(animation);
    if (drawTask) await drawTask.catch(() => {});
    cache.clear();
    if (window.__bmuxDocumentInspection === inspection) delete window.__bmuxDocumentInspection;
    try { adapter.reset(); } finally { adapter.free(); }
    expire();
  }
}

const delivery = new URLSearchParams(location.search);
const browserTask = delivery.get('delivery') === 'document'
  ? runDocument(delivery.get('adapter'))
  : run();
browserTask.catch(() => { if (!stopped) status.textContent = 'Browser renderer could not start.'; });
