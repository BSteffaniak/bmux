import { mountCanvasViewer } from './canvas-viewer.js';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export function revision(value) {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,19})$/.test(value)
      || BigInt(value) > 18446744073709551615n) throw new Error('invalid cursor');
  return value;
}
export function validateStatus(value) {
  if (value.type !== 'state' || !['offline', 'starting', 'live', 'paused'].includes(value.state)) {
    throw new Error('invalid state');
  }
  revision(value.fence);
  if (value.epoch !== null && (typeof value.epoch !== 'string' || !UUID.test(value.epoch))) {
    throw new Error('invalid epoch');
  }
  if (value.state !== 'offline' && !value.epoch) throw new Error('missing epoch');
  if (value.state !== 'offline' && (!Number.isInteger(value.lease_ms) || value.lease_ms !== 15000
      || !Number.isInteger(value.lease_remaining_ms) || value.lease_remaining_ms <= 0
      || value.lease_remaining_ms > value.lease_ms)) {
    throw new Error('invalid lease');
  }
  return value;
}

// Diagnostics are a local observation, never proof of physical display or ongoing
// upstream delivery. Only allowlisted labels and bounded numeric cursors survive.
export function createDeliveryDiagnostics(now = () => performance.now(), assetBase = '') {
  const limit = 64;
  // Only content-addressed bundle identities from the existing asset URL, not
  // arbitrary paths, hosts, query strings, peer metadata or claimed build text.
  const browserBuild = typeof assetBase === 'string'
    ? /\/live-assets\/([0-9a-f]{64})\/$/.exec(assetBase)?.[1] ?? null : null;
  const counts = { receive: 0, bytes: 0, decoded: 0, submitted: 0, ackSent: 0, repairs: 0, reconnects: 0 };
  const times = { receive: null, decoded: null, submitted: null, ackSent: null };
  let history = [], connection = 'unknown', source = 'unknown', pendingSince = null, ackSince = null;
  let retained = false, repairing = false, generation = null, generationNumber = 0, cursor = null, ack = null;
  let reason = 'none';
  const tick = () => Math.max(0, Math.floor(now()));
  const add = (key, amount = 1) => { counts[key] = Math.min(Number.MAX_SAFE_INTEGER, counts[key] + amount); };
  function record(event, progress = null) {
    history.push({ event, at: tick(), ...(progress ? { cursor: { ...progress } } : {}) });
    if (history.length > limit) history.shift();
  }
  const reasons = new Set(['none', 'authority-change', 'paused', 'offline', 'lease-expired',
    'invalid-stream', 'invalid-frame', 'decode-or-submit-failed', 'socket-error', 'peer-closed',
    'reconnect-scheduled', 'connecting', 'disposed', 'ack-send-failed']);
  return {
    connection(value) {
      if (!['connecting', 'open', 'closed', 'disposed'].includes(value)) return;
      connection = value;
      record(value);
    },
    source(value) { if (['offline', 'starting', 'live', 'paused'].includes(value)) source = value; },
    reason(value) { reason = reasons.has(value) ? value : 'invalid-stream'; record(reason); },
    reconnect() { add('reconnects'); record('reconnect-scheduled'); },
    receive(bytes = 0) {
      add('receive');
      if (Number.isSafeInteger(bytes) && bytes > 0) add('bytes', bytes);
      times.receive = tick(); record('receive');
    },
    pending() { pendingSince = tick(); record('pending'); },
    decoded() { add('decoded'); times.decoded = tick(); record('decoded'); },
    submitted(identity) {
      // A local ordinal deliberately replaces the opaque peer generation.
      if (identity.generation !== generation) { generation = identity.generation; generationNumber = Math.min(Number.MAX_SAFE_INTEGER, generationNumber + 1); }
      cursor = { generation: generationNumber, revision: revision(identity.revision) };
      add('submitted'); times.submitted = tick(); pendingSince = null; ackSince = tick(); retained = true; repairing = false;
      record('renderer-submitted', cursor);
    },
    ack() { ack = cursor && { ...cursor }; ackSince = null; add('ackSent'); times.ackSent = tick(); record('ack-sent', ack); },
    clear() { pendingSince = ackSince = null; retained = false; repairing = false; generation = null; source = 'unknown'; },
    repair() { add('repairs'); repairing = true; pendingSince = tick(); record('repair-requested'); },
    snapshot() {
      const time = tick();
      const age = value => value === null ? null : Math.max(0, time - value);
      const pendingAge = age(pendingSince);
      const progress = connection === 'disposed' ? 'disposed'
        : connection !== 'open' ? 'unknown'
        : repairing ? (pendingAge >= 5000 ? 'repair-stalled' : 'repair-pending')
        : pendingAge !== null ? (pendingAge >= 5000 ? 'pending-stalled' : 'pending')
        : ackSince !== null ? (age(ackSince) >= 5000 ? 'ack-stalled' : 'ack-pending')
        : source === 'live' || source === 'starting' ? 'idle' : source;
      return {
        schema: 1, connection, source, progress, reason, retained, localPendingWarningMs: 5000,
        evidence: 'historical local observations; renderer submission only, not physical display or ongoing live delivery',
        builds: { browser: browserBuild, renderer: null, relay: null, source: null },
        counts: { ...counts }, agesMs: { ...Object.fromEntries(Object.entries(times).map(([key, value]) => [key, age(value)])), pending: pendingAge, ackPending: age(ackSince) },
        submitted: cursor && { ...cursor }, ackSent: ack && { ...ack },
        history: history.map(({ event, at, cursor }) => ({ event, ageMs: age(at), ...(cursor ? { cursor: { ...cursor } } : {}) })),
      };
    },
  };
}

// Exactly one incoming frame is retained until it has been admitted and ACKed.
// A peer violating the credit contract is disconnected, never silently dropped.
export async function mountLiveViewer({ container, status, apiBase = '/', assetBase,
  viewerFactory = mountCanvasViewer, WebSocketImpl = globalThis.WebSocket,
  requestFrame = callback => requestAnimationFrame(callback), cancelFrame = id => cancelAnimationFrame(id),
  setTimer = setTimeout, clearTimer = clearTimeout, now = () => performance.now(),
  maxFps = 60, autoStart = true }) {
  if (!Number.isFinite(maxFps) || maxFps <= 0 || maxFps > 60) throw new Error('invalid max FPS');
  const diagnostics = createDeliveryDiagnostics(now, assetBase);
  const viewer = await viewerFactory({ container, assetBase });
  const url = new URL('/v1/viewer/ws', new URL(apiBase, container.ownerDocument.baseURI));
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  let socket, reconnect, raf, frameTimer, leaseTimer;
  let disposed = false, ticket = 0, state = null, pending = null, busy = false, repairing = false;
  let lastPaint = -Infinity;
  const counters = { bytes: 0, frames: 0, repair: 0 };
  const count = key => { counters[key] = Math.min(Number.MAX_SAFE_INTEGER, counters[key] + 1); };
  function invalidate(label) {
    ticket++;
    pending = null;
    repairing = false;
    if (raf !== undefined) cancelFrame(raf);
    if (frameTimer !== undefined) clearTimer(frameTimer);
    raf = frameTimer = undefined;
    viewer.clear();
    diagnostics.clear();
    status.textContent = label;
  }
  function send(value, connection = socket) {
    if (connection?.readyState !== 1) return false;
    connection.send(JSON.stringify(value));
    return true;
  }
  function repair(connection, reason = 'decode-or-submit-failed') {
    invalidate('Repairing broadcast…');
    diagnostics.reason(reason);
    diagnostics.repair();
    if (state) diagnostics.source(state.state);
    repairing = true;
    count('repair');
    try {
      if (!send({ type: 'repair' }, connection)) {
        diagnostics.reason('socket-error');
        connection.close();
      }
    } catch {
      diagnostics.reason('socket-error');
      connection.close();
    }
  }
  function schedule() {
    if (disposed || busy || !pending || raf !== undefined || frameTimer !== undefined) return;
    raf = requestFrame(async () => {
      raf = undefined;
      if (!pending || disposed) return;
      // RAF timestamps describe the animation timeline, not when an async
      // decoder actually admitted its canvas. Pace against the last completion
      // using one monotonic clock so a slow paint cannot compress the next gap.
      const wait = 1000 / maxFps - (now() - lastPaint);
      if (wait > 0) {
        frameTimer = setTimer(() => { frameTimer = undefined; schedule(); }, wait);
        return;
      }
      const frame = pending;
      pending = null;
      busy = true;
      const current = () => !disposed && ticket === frame.ticket && socket === frame.socket;
      try {
        const accepted = await viewer.present(frame.bytes, current, () => {
          if (current()) diagnostics.decoded();
        });
        if (!current()) return;
        const identity = viewer.identity;
        if (!accepted || !identity || typeof identity.generation !== 'string') throw new Error('unadmitted frame');
        revision(identity.revision);
        diagnostics.submitted(identity);
        // This records a local send, not a server receipt or physical display.
        try {
          if (send({ type: 'ack', generation: identity.generation, revision: identity.revision }, frame.socket)) diagnostics.ack();
          else diagnostics.reason('ack-send-failed');
        } catch {
          diagnostics.reason('ack-send-failed');
          throw new Error('ack send failed');
        }
        repairing = false;
        lastPaint = now();
        count('frames');
        status.textContent = 'Read-only · frame submitted; waiting for updates';
      } catch {
        if (current()) repair(frame.socket);
      } finally {
        busy = false;
        schedule();
      }
    });
  }
  function connect() {
    if (disposed || socket) return;
    diagnostics.connection('connecting');
    diagnostics.reason('connecting');
    const connection = new WebSocketImpl(url.href);
    socket = connection;
    connection.binaryType = 'arraybuffer';
    connection.onopen = () => {
      if (!disposed && socket === connection) diagnostics.connection('open');
    };
    connection.onmessage = event => {
      if (disposed || socket !== connection) return;
      diagnostics.receive(event.data instanceof ArrayBuffer ? event.data.byteLength : 0);
      try {
        if (typeof event.data === 'string') {
          if (event.data.length > 4096) throw new Error('control bound');
          const next = validateStatus(JSON.parse(event.data));
          if (state && BigInt(next.fence) < BigInt(state.fence)) throw new Error('stale authority');
          if (!state || state.epoch !== next.epoch || state.fence !== next.fence
              || !['live', 'starting'].includes(next.state)) {
            invalidate(next.state === 'paused' ? 'Broadcast paused' : next.state === 'offline'
              ? 'Broadcast offline' : 'Connecting to broadcast…');
            diagnostics.reason(next.state === 'paused' ? 'paused' : next.state === 'offline' ? 'offline' : 'authority-change');
          }
          state = next;
          diagnostics.connection('open');
          diagnostics.source(next.state);
          clearTimer(leaseTimer);
          if (next.state !== 'offline') leaseTimer = setTimer(() => {
            if (socket !== connection) return;
            invalidate('Broadcast lease expired');
            diagnostics.reason('lease-expired');
            connection.close();
          }, next.lease_remaining_ms);
          return;
        }
        if (!(event.data instanceof ArrayBuffer) || event.data.byteLength === 0
            || event.data.byteLength > 8 * 1024 * 1024 || !state
            || !['live', 'starting'].includes(state.state)) throw new Error('invalid frame');
        if (pending || busy) throw new Error('frame credit exceeded');
        const bytes = new Uint8Array(event.data);
        if (bytes.length < 5 || bytes[0] !== 66 || bytes[1] !== 77 || bytes[2] !== 87
            || bytes[3] !== 80 || bytes[4] !== 5) throw new Error('incompatible presentation');
        counters.bytes = Math.min(Number.MAX_SAFE_INTEGER, counters.bytes + bytes.length);
        pending = { bytes, socket: connection, ticket };
        diagnostics.pending();
        schedule();
      } catch {
        if (typeof event.data === 'string' || pending || busy || repairing) {
          diagnostics.reason('invalid-stream');
          connection.close(1002, 'invalid stream');
        } else repair(connection, 'invalid-frame');
      }
    };
    connection.onclose = () => {
      if (socket !== connection) return;
      socket = null;
      clearTimer(leaseTimer);
      leaseTimer = undefined;
      state = null;
      invalidate('Broadcast disconnected');
      diagnostics.connection('closed');
      diagnostics.reason('peer-closed');
      if (!disposed) {
        diagnostics.reconnect();
        reconnect = setTimer(connect, 1000);
      }
    };
    connection.onerror = () => {
      if (disposed || socket !== connection) return;
      diagnostics.reason('socket-error');
      connection.close();
    };
  }
  status.textContent = 'Connecting to broadcast…';
  if (autoStart) connect();
  return {
    connect,
    get counters() { return { ...counters }; },
    get diagnostics() { return diagnostics.snapshot(); },
    exportDiagnostics() { return JSON.stringify(diagnostics.snapshot(), null, 2); },
    dispose() {
      disposed = true;
      clearTimer(reconnect);
      clearTimer(leaseTimer);
      invalidate('Broadcast offline');
      socket?.close();
      socket = null;
      diagnostics.connection('disposed');
      diagnostics.reason('disposed');
      viewer.dispose();
    },
  };
}
