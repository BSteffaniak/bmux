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

// Exactly one incoming frame is retained until it has been admitted and ACKed.
// A peer violating the credit contract is disconnected, never silently dropped.
export async function mountLiveViewer({ container, status, apiBase = '/', assetBase,
  viewerFactory = mountCanvasViewer, WebSocketImpl = globalThis.WebSocket,
  requestFrame = callback => requestAnimationFrame(callback), cancelFrame = id => cancelAnimationFrame(id),
  setTimer = setTimeout, clearTimer = clearTimeout, now = () => performance.now(),
  maxFps = 60, autoStart = true }) {
  if (!Number.isFinite(maxFps) || maxFps <= 0 || maxFps > 60) throw new Error('invalid max FPS');
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
    status.textContent = label;
  }
  function send(value, connection = socket) {
    if (connection?.readyState === 1) connection.send(JSON.stringify(value));
  }
  function repair(connection) {
    invalidate('Repairing broadcast…');
    repairing = true;
    count('repair');
    send({ type: 'repair' }, connection);
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
        const accepted = await viewer.present(frame.bytes, current);
        if (!current()) return;
        const identity = viewer.identity;
        if (!accepted || !identity || typeof identity.generation !== 'string') throw new Error('unadmitted frame');
        revision(identity.revision);
        send({ type: 'ack', generation: identity.generation, revision: identity.revision }, frame.socket);
        repairing = false;
        lastPaint = now();
        count('frames');
        status.textContent = 'Live · read-only';
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
    const connection = new WebSocketImpl(url.href);
    socket = connection;
    connection.binaryType = 'arraybuffer';
    connection.onmessage = event => {
      if (disposed || socket !== connection) return;
      try {
        if (typeof event.data === 'string') {
          if (event.data.length > 4096) throw new Error('control bound');
          const next = validateStatus(JSON.parse(event.data));
          if (state && BigInt(next.fence) < BigInt(state.fence)) throw new Error('stale authority');
          if (!state || state.epoch !== next.epoch || state.fence !== next.fence
              || !['live', 'starting'].includes(next.state)) {
            invalidate(next.state === 'paused' ? 'Broadcast paused' : next.state === 'offline'
              ? 'Broadcast offline' : 'Connecting to broadcast…');
          }
          state = next;
          clearTimer(leaseTimer);
          if (next.state !== 'offline') leaseTimer = setTimer(() => {
            if (socket !== connection) return;
            invalidate('Broadcast lease expired');
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
        schedule();
      } catch {
        if (typeof event.data === 'string' || pending || busy || repairing) connection.close(1002, 'invalid stream');
        else repair(connection);
      }
    };
    connection.onclose = () => {
      if (socket !== connection) return;
      socket = null;
      clearTimer(leaseTimer);
      leaseTimer = undefined;
      state = null;
      invalidate('Broadcast disconnected');
      if (!disposed) reconnect = setTimer(connect, 1000);
    };
    connection.onerror = () => connection.close();
  }
  status.textContent = 'Connecting to broadcast…';
  if (autoStart) connect();
  return {
    connect,
    get counters() { return { ...counters }; },
    dispose() {
      disposed = true;
      clearTimer(reconnect);
      clearTimer(leaseTimer);
      invalidate('Broadcast offline');
      socket?.close();
      socket = null;
      viewer.dispose();
    },
  };
}
