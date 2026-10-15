// Deployment artifact: native ES module, no build step or runtime dependencies.
// BMWP3 positional admission mirrors packages/web-renderer/src/wire.rs; Rust
// producer/viewer still own full semantic metadata/image/render validation.
export const MAX_FRAME_BYTES = 8 * 1024 * 1024;
const bundleEnabled = env => env.DELTA_BUNDLES === 'fanout';
export const LEASE_MS = 15000;
export const BUDGETS = Object.freeze({ requests: 16, uploads: 1, viewers: 4, bodyTimeoutMs: 10000 });
const U64 = (1n << 64n) - 1n;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const CHANNEL = /^[a-z0-9][a-z0-9-]{0,47}$/;
const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
class Rejection extends Error {
  constructor(status) { super('request rejected'); this.status = status; }
}
function reject(status = 400) { throw new Rejection(status); }
function response(status, data, extra = {}) {
  const headers = {
    'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff',
    'Referrer-Policy': 'no-referrer', 'Content-Security-Policy': "default-src 'none'; frame-ancestors 'none'",
    ...extra,
  };
  if (data !== undefined && !(data instanceof Uint8Array) && !(data instanceof ReadableStream)) {
    headers['Content-Type'] = 'application/json'; data = JSON.stringify(data);
  }
  return new Response(data, { status, headers });
}
function cursor(value, positive = false) {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,19})$/.test(value)) reject();
  const n = BigInt(value);
  if (n > U64 || (positive && n === 0n)) reject();
  return n;
}
function configured(env) {
  const channel = env.CHANNEL ?? 'main';
  if (!CHANNEL.test(channel) || !/^[a-f0-9]{64}$/i.test(env.PUBLISHER_KEY_SHA256 ?? '')) reject(503);
  return channel;
}
function route(request, env, local = false) {
  const url = new URL(request.url);
  if (url.protocol !== 'https:' && !(local && url.protocol === 'http:' &&
      ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))) reject(400);
  if (url.username || url.password) reject();
  const match = /^\/api\/live\/([a-z0-9][a-z0-9-]{0,47})\/(status|presentation|health|publisher\/(start|frame|heartbeat|pause|resume|stop))$/.exec(url.pathname);
  if (!match || match[1] !== configured(env)) reject(404);
  const action = match[2];
  const method = action === 'publisher/frame' ? 'PUT' : action.startsWith('publisher/') ? 'POST' : 'GET';
  if (request.method !== method) reject(405);
  if (url.search && action !== 'presentation') reject();
  if (action === 'presentation') {
    const allowed = request.headers.get('X-Bmux-Capabilities') === 'bmwp-delta-v1'
      ? ['epoch', 'after', 'generation', 'base'] : ['epoch', 'after'];
    for (const key of url.searchParams.keys()) if (!allowed.includes(key) || url.searchParams.getAll(key).length !== 1) reject();
    if (url.searchParams.has('base')) cursor(url.searchParams.get('base'));
    if (url.searchParams.has('generation') && encoder.encode(url.searchParams.get('generation')).length > 128) reject();
  }
  return { url, action };
}
async function digest(bytes) {
  return new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
}
function hex(bytes) { return Array.from(bytes, b => b.toString(16).padStart(2, '0')).join(''); }
async function authorize(request, env) {
  const header = request.headers.get('Authorization') ?? '';
  if (header.length > 4096 || !/^Bearer [\x21-\x7e]+$/.test(header)) reject(401);
  const actual = await digest(encoder.encode(header.slice(7)));
  const expected = env.PUBLISHER_KEY_SHA256;
  let difference = 0;
  // Fixed 32-byte XOR, no digest-prefix or secret-dependent early exits.
  for (let i = 0; i < 32; i++) difference |= actual[i] ^ parseInt(expected.slice(i * 2, i * 2 + 2), 16);
  if (difference !== 0) reject(401);
}
export async function readBounded(request, limit, timeoutMs = BUDGETS.bodyTimeoutMs) {
  if (request.headers.has('Content-Encoding')) reject(415);
  const length = request.headers.get('Content-Length');
  if (length !== null && (!/^\d+$/.test(length) || BigInt(length) > BigInt(limit))) reject(413);
  if (!request.body) return new Uint8Array();
  const reader = request.body.getReader();
  // Fixed allocation bounds peak body retention and avoids a chunk-count queue.
  const buffer = new Uint8Array(limit);
  let used = 0, timer;
  const timeout = new Promise((_, fail) => { timer = setTimeout(() => fail(new Rejection(408)), timeoutMs); });
  try {
    while (true) {
      const { value, done } = await Promise.race([reader.read(), timeout]);
      if (done) break;
      if (!(value instanceof Uint8Array) || used + value.byteLength > limit) reject(413);
      buffer.set(value, used); used += value.byteLength;
    }
    if (length !== null && BigInt(length) !== BigInt(used)) reject();
    return buffer.subarray(0, used);
  } catch (error) {
    // Cancellation need not settle if the peer's stream is malicious/stalled.
    void reader.cancel().catch(() => {});
    throw error;
  } finally { clearTimeout(timer); reader.releaseLock(); }
}

// Schema-specific bounded cursor, not a second rendering implementation.
class Wire {
  constructor(bytes) { this.bytes = bytes; this.at = 5; this.entries = 0; }
  byte() { if (this.at >= this.bytes.length) reject(); return this.bytes[this.at++]; }
  uint(max = U64) {
    let n = 0n;
    for (let shift = 0n; shift < 70n; shift += 7n) {
      const b = this.byte(); n |= BigInt(b & 127) << shift;
      if (!(b & 128)) { if (n > max) reject(); return n; }
    }
    reject();
  }
  small(max = 0xffffffff) { return Number(this.uint(BigInt(max))); }
  bool() { const b = this.byte(); if (b > 1) reject(); return b === 1; }
  option(fn) { return this.bool() ? fn() : null; }
  blob(max = MAX_FRAME_BYTES) {
    const n = this.small(max); if (n > this.bytes.length - this.at) reject();
    const bytes = this.bytes.subarray(this.at, this.at + n); this.at += n; return bytes;
  }
  text(max = MAX_FRAME_BYTES) { return decoder.decode(this.blob(max)); }
  seq(fn, max = 262144) {
    const n = this.small(max); this.entries += n;
    if (this.entries > 2097152) reject();
    for (let i = 0; i < n; i++) fn(i);
    return n;
  }
  rect() { return Array.from({ length: 4 }, () => this.small(65535)); }
  cellWidth() {
    const width = this.byte(), continuation = this.bool();
    if (width > 2 || (continuation ? width !== 0 : width === 0)) reject();
  }
  color() { const kind = this.small(18); if (kind === 17) this.byte(); if (kind === 18) { this.byte(); this.byte(); this.byte(); } }
  requirement() {
    const id = this.text(128), mime = this.text(64);
    if (!/^[a-f0-9]{64}$/.test(id)) reject();
    const width = this.small(), height = this.small();
    const format = this.option(() => this.small(1));
    if (!width || !height || (format === null ? mime !== 'image/png' : mime !== 'application/octet-stream')) reject();
    return { id, mime, width, height, format };
  }
}
export function validateSnapshot(bytes) { return validatePresentation(bytes, false); }
function validatePresentation(bytes, allowDelta) {
  if (bytes.length < 5 || bytes[0] !== 66 || bytes[1] !== 77 || bytes[2] !== 87 || bytes[3] !== 80 || bytes[4] !== 3) reject(415);
  const w = new Wire(bytes);
  const generation = w.text(128); if (!generation || /[\u0000-\u001f\u007f]/.test(generation)) reject();
  const revision = w.uint().toString();
  const base = w.option(() => ({ generation: w.text(128), revision: w.uint().toString() }));
  if (base && (!allowDelta || base.generation !== generation || BigInt(base.revision) >= BigInt(revision))) reject();
  const [x, y, width, height] = w.rect();
  const cells = width * height;
  if (cells > 262144 || x + width > 65535 || y + height > 65535) reject();
  const styles = w.seq(() => { w.option(() => w.color()); w.option(() => w.color()); w.small(65535); }, cells);
  const glyphs = w.seq(() => w.text(128), cells);
  const rows = new Set();
  w.seq(() => {
    const row = w.small(65535); if (row >= height || rows.has(row)) reject(); rows.add(row);
    let columns = 0;
    w.seq(() => {
      const kind = w.small(2), style = w.small(); if (style >= styles) reject();
      if (kind === 0) {
        if (w.small() >= glyphs) reject();
        w.cellWidth(); const count = w.small(65535); if (!count) reject(); columns += count;
      } else if (kind === 1) {
        const ascii = w.blob(); if (!ascii.length) reject(); for (const b of ascii) if (b < 32 || b > 126) reject(); columns += ascii.length;
      } else {
        const count = w.seq(() => { if (w.small() >= glyphs) reject(); w.cellWidth(); }, width);
        if (!count) reject(); columns += count;
      }
      if (columns > width) reject();
    }, width);
    if (columns !== width) reject();
  }, height);
  if (!base && rows.size !== height) reject();
  const metadata = w.bool();
  if (!base && !metadata) reject();
  if (metadata && w.blob().length === 0) reject();
  const required = new Map();
  const images = w.bool();
  if (!base && !images) reject();
  if (images) w.seq(() => {
    w.text(256); const r = w.requirement(); required.set(r.id, r);
    w.rect(); w.small(1);
    w.option(() => { w.small(); w.small(); w.small(65535); w.small(65535); });
    w.rect(); w.small(1);
  }, 4096);
  const resources = new Map();
  w.seq(() => {
    const r = w.requirement(); if (resources.has(r.id)) reject();
    const payload = w.blob(); if (!payload.length) reject();
    resources.set(r.id, r);
  }, 256);
  for (const [id, r] of required) {
    const supplied = resources.get(id);
    if ((!supplied && !base) || (supplied && JSON.stringify(supplied) !== JSON.stringify(r))) reject();
  }
  if (w.at !== bytes.length) reject();
  return { generation, revision, base, area: [x, y, width, height] };
}

async function compressed(bytes) {
  if (typeof CompressionStream !== 'function' || bytes.length < 1024) return null;
  const stream = new Blob([bytes]).stream().pipeThrough(new CompressionStream('gzip'));
  const reader = stream.getReader();
  const output = new Uint8Array(bytes.length);
  let size = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return size < bytes.length ? output.slice(0, size) : null;
      if (size + value.length >= bytes.length) { void reader.cancel().catch(() => {}); return null; }
      output.set(value, size); size += value.length;
    }
  } catch { return null; }
  finally { reader.releaseLock(); }
}
function blank() {
  return { version: 1, state: 'offline', epoch: null, publisher_id: null, revision: '0',
    lease_expires_at: null, revoked: null, frame_hash: null, fence: 0 };
}
function validMetadata(m) {
  return m && m.version === 1 && ['offline', 'starting', 'live', 'paused'].includes(m.state) &&
    (m.epoch === null || UUID.test(m.epoch)) && (m.revoked === null || UUID.test(m.revoked)) &&
    (m.publisher_id === null || UUID.test(m.publisher_id)) && typeof m.revision === 'string' &&
    /^(0|[1-9][0-9]{0,19})$/.test(m.revision) && BigInt(m.revision) <= U64 &&
    (m.frame_hash === null || /^[a-f0-9]{64}$/.test(m.frame_hash)) &&
    Number.isSafeInteger(m.fence) && m.fence >= 0 &&
    (m.state === 'offline' ? m.epoch === null && m.publisher_id === null && m.lease_expires_at === null :
      UUID.test(m.epoch) && UUID.test(m.publisher_id) && Number.isSafeInteger(m.lease_expires_at));
}
export class ChannelHandler {
  constructor(storage, env, { now = Date.now, local = false, bodyTimeoutMs = BUDGETS.bodyTimeoutMs } = {}) {
    this.storage = storage; this.env = env; this.now = now; this.local = local; this.bodyTimeoutMs = bodyTimeoutMs;
    this.delta = null; this.identity = null; this.compressedFull = null;
    this.counters = { wire_bytes: 0, full: 0, delta: 0, repair: 0,
      upload_wire_bytes: 0, upload_full_bytes: 0, bundle_overhead_bytes: 0 };
    this.frame = null; this.active = 0; this.uploads = 0; this.viewers = 0; this.deliveries = new Set(); this.tail = Promise.resolve();
    this.ready = this.load();
  }
  async load() {
    const saved = await this.storage.get('metadata');
    if (saved !== undefined && !validMetadata(saved)) throw new Error('unsupported durable relay state');
    this.meta = saved ?? blank();
    if (this.meta.state === 'live') {
      await this.commit({ ...this.meta, state: 'starting', fence: this.meta.fence + 1 }, null);
    }
  }
  async commit(meta, frame = this.frame) {
    // Store only small authority metadata, never frame bytes in SQLite values.
    const priorEpoch = this.meta?.epoch;
    await this.storage.put('metadata', meta);
    this.meta = meta; this.frame = frame;
    if (frame === null || meta.epoch !== priorEpoch) { this.delta = null; this.identity = null; this.compressedFull = null; }
    if (meta.epoch === null || meta.state !== 'live' || meta.epoch !== priorEpoch) {
      for (const invalidate of [...this.deliveries]) invalidate();
    }
  }
  serialized(fn) {
    const result = this.tail.then(async () => { await this.ready; await this.expire(); return fn(); });
    this.tail = result.catch(() => {}); return result;
  }
  async expire() {
    if (this.meta.epoch !== null && this.meta.lease_expires_at <= this.now()) {
      await this.commit({ ...blank(), revoked: this.meta.epoch, fence: this.meta.fence + 1 }, null);
    }
  }
  owner(request) {
    const epoch = request.headers.get('X-Bmux-Epoch');
    if (!epoch || epoch !== this.meta.epoch) reject(409);
    return epoch;
  }
  async fetch(request) {
    if (this.active >= BUDGETS.requests) return response(429, { error: 'request rejected' }, { 'Retry-After': '1' });
    this.active++;
    try {
      const { action, url } = route(request, this.env, this.local);
      if (action.startsWith('publisher/')) await authorize(request, this.env);
      if (action === 'publisher/frame') return await this.upload(request);
      let start;
      if (action === 'publisher/start') {
        if (request.headers.get('Content-Type')?.split(';')[0].trim() !== 'application/json') reject(415);
        try { start = JSON.parse(decoder.decode(await readBounded(request, 1024, this.bodyTimeoutMs))); } catch (e) { if (e instanceof Rejection) throw e; reject(); }
        if (start?.protocol !== 1 || start?.renderer !== 'bmwp-3') reject(415);
        if (typeof start.publisher_id !== 'string' || !UUID.test(start.publisher_id) || Object.keys(start).some(k => !['protocol', 'renderer', 'publisher_id'].includes(k))) reject();
      } else if (action.startsWith('publisher/')) {
        if ((await readBounded(request, 0, this.bodyTimeoutMs)).length) reject();
      }
      return await this.serialized(async () => {
        route(request, this.env, this.local);
        if (action.startsWith('publisher/')) await authorize(request, this.env);
        await this.expire();
        const m = this.meta;
        if (action === 'health') return response(200, { protocol: 1, renderer: 'bmwp-3' });
        if (action === 'status') return response(200, { protocol: 1, renderer: 'bmwp-3', state: m.state,
          epoch: m.epoch, revision: m.revision, lease_expires_at: m.lease_expires_at },
          request.headers.get('X-Bmux-Capabilities') === 'bmwp-delta-v1'
            ? { 'X-Bmux-Delivery-Counters': JSON.stringify(this.counters) } : {});
        if (action === 'presentation') {
          if (url.searchParams.get('epoch') !== m.epoch) reject(409);
          const after = url.searchParams.has('after') ? cursor(url.searchParams.get('after')) : null;
          if (m.state !== 'live' || !this.frame) reject(503);
          const negotiated = request.headers.get('X-Bmux-Capabilities') === 'bmwp-delta-v1';
          const exactCurrent = this.identity && url.searchParams.get('generation') === this.identity.generation
            && url.searchParams.get('base') === this.identity.revision;
          if (after !== null && after === BigInt(m.revision) && (!negotiated || exactCurrent)) return response(204);
          if (this.viewers >= BUDGETS.viewers) reject(429);
          const useDelta = negotiated && this.delta && after?.toString() === this.delta.baseRelay
            && url.searchParams.get('generation') === this.delta.base.generation
            && url.searchParams.get('base') === this.delta.base.revision;
          let selected = useDelta ? this.delta.bytes : this.frame;
          const packed = useDelta ? this.delta.compressed : this.compressedFull;
          const headers = { 'Content-Type': 'application/octet-stream', 'X-Bmux-Epoch': m.epoch, 'X-Snapshot-Revision': m.revision };
          if (request.headers.get('X-Bmux-Compression') === 'gzip-v1' && packed) {
            headers['X-Bmux-Compression'] = 'gzip-v1';
            selected = packed;
          }
          if (negotiated && this.identity) {
            Object.assign(headers, { 'X-Bmux-Capabilities': 'bmwp-delta-v1',
              'X-Bmux-Frame': useDelta ? 'delta' : 'full', 'X-Bmux-Generation': encodeURIComponent(this.identity.generation),
              'X-Bmux-Frame-Revision': this.identity.revision });
            if (useDelta) Object.assign(headers, { 'X-Bmux-Base-Revision': this.delta.baseRelay,
              'X-Bmux-Base-Frame-Revision': this.delta.base.revision });
          }
          this.counters[useDelta ? 'delta' : 'full'] = Math.min(Number.MAX_SAFE_INTEGER, this.counters[useDelta ? 'delta' : 'full'] + 1);
          if (negotiated && !useDelta) this.counters.repair = Math.min(Number.MAX_SAFE_INTEGER, this.counters.repair + 1);
          this.viewers++;
          let bytes = selected, offset = 0, released = false, timer, invalidate;
          const deliveryEpoch = m.epoch;
          const handler = this;
          const release = () => {
            if (released) return;
            released = true; clearTimeout(timer); bytes = null; this.viewers--; this.deliveries.delete(invalidate);
          };
          const stream = new ReadableStream({
            start(controller) {
              invalidate = () => { if (!released) { controller.error(new Error('delivery invalidated')); release(); } };
              handler.deliveries.add(invalidate);
              timer = setTimeout(invalidate, BUDGETS.bodyTimeoutMs);
            },
            pull(controller) {
              if (released) return;
              if (handler.meta.epoch !== deliveryEpoch || handler.meta.state !== 'live' || handler.meta.lease_expires_at <= handler.now()) {
                invalidate(); return;
              }
              if (offset === bytes.length) { controller.close(); release(); return; }
              const end = Math.min(offset + 65536, bytes.length);
              handler.counters.wire_bytes = Math.min(Number.MAX_SAFE_INTEGER, handler.counters.wire_bytes + end - offset);
              controller.enqueue(bytes.slice(offset, end)); offset = end;
            },
            cancel: release,
          }, { highWaterMark: 0 });
          return response(200, stream, headers);
        }
        if (action === 'publisher/start') {
          if (m.epoch !== null && m.publisher_id !== start.publisher_id.toLowerCase()) reject(409);
          if (m.epoch === null) await this.commit({ ...blank(), revoked: m.revoked, state: 'starting',
            epoch: crypto.randomUUID(), publisher_id: start.publisher_id.toLowerCase(), lease_expires_at: this.now() + LEASE_MS, fence: m.fence + 1 }, null);
          return response(200, { protocol: 1, renderer: 'bmwp-3', epoch: this.meta.epoch, lease_ms: LEASE_MS },
            bundleEnabled(this.env) && request.headers.get('X-Bmux-Capabilities') === 'bmwp-delta-v1' ? { 'X-Bmux-Capabilities': 'bmwp-delta-v1' } : {});
        }
        if (action === 'publisher/stop' && request.headers.get('X-Bmux-Epoch') === m.revoked) return response(204);
        this.owner(request);
        const next = { ...m, lease_expires_at: this.now() + LEASE_MS };
        if (action === 'publisher/pause') { next.state = 'paused'; next.fence++; }
        if (action === 'publisher/resume') { if (m.state !== 'paused') reject(409); next.state = 'starting'; next.fence++; }
        if (action === 'publisher/stop') {
          await this.commit({ ...blank(), revoked: m.epoch, fence: m.fence + 1 }, null);
        } else await this.commit(next, action === 'publisher/heartbeat' ? this.frame : null);
        return response(204);
      });
    } catch (e) {
      return response(e instanceof Rejection ? e.status : 503, { error: 'request rejected' });
    } finally { this.active--; }
  }
  async upload(request) {
    if (request.headers.get('Content-Type') !== 'application/octet-stream') reject(415);
    const revision = cursor(request.headers.get('X-Bmux-Revision'), true).toString();
    if (this.uploads >= BUDGETS.uploads) reject(429);
    this.uploads++;
    try {
      const admitted = await this.serialized(() => {
        const epoch = this.owner(request);
        if (this.meta.state === 'paused') reject(409);
        if (BigInt(revision) < BigInt(this.meta.revision)) reject(409);
        return { epoch, fence: this.meta.fence, identity: this.identity, revision: this.meta.revision };
      });
      const bundled = request.headers.get('X-Bmux-Capabilities') === 'bmwp-delta-v1';
      if (bundled && !bundleEnabled(this.env)) reject(415);
      const body = await readBounded(request, bundled ? 2 * MAX_FRAME_BYTES + 4 : MAX_FRAME_BYTES, this.bodyTimeoutMs);
      this.counters.upload_wire_bytes = Math.min(Number.MAX_SAFE_INTEGER, this.counters.upload_wire_bytes + body.length);
      let bytes = body, delta = null, identity;
      try {
        if (bundled) {
          if (body.length < 4) reject();
          const size = new DataView(body.buffer, body.byteOffset, 4).getUint32(0, true);
          if (!size || size > MAX_FRAME_BYTES || body.length <= 4 + size || body.length - 4 - size >= size) reject();
          bytes = body.slice(4, 4 + size);
          const patch = body.slice(4 + size);
          identity = validateSnapshot(bytes);
          const next = validatePresentation(patch, true);
          const baseRelay = cursor(request.headers.get('X-Bmux-Base-Revision'), true).toString();
          if (!next.base || next.generation !== identity.generation || next.revision !== identity.revision
              || JSON.stringify(next.area) !== JSON.stringify(identity.area)) reject(409);
          const retry = revision === admitted.revision && admitted.identity
            && identity.generation === admitted.identity.generation && identity.revision === admitted.identity.revision;
          if (!retry && (!admitted.identity || baseRelay !== admitted.revision
              || next.base.generation !== admitted.identity.generation || next.base.revision !== admitted.identity.revision
              || JSON.stringify(next.area) !== JSON.stringify(admitted.identity.area))) reject(409);
          delta = retry ? null : { bytes: patch, base: next.base, baseRelay };
        } else identity = validateSnapshot(bytes);
      } catch (e) { if (e instanceof Rejection) throw e; reject(); }
      this.counters.upload_full_bytes = Math.min(Number.MAX_SAFE_INTEGER, this.counters.upload_full_bytes + bytes.length);
      this.counters.bundle_overhead_bytes = Math.min(Number.MAX_SAFE_INTEGER, this.counters.bundle_overhead_bytes + body.length - bytes.length);
      const hash = hex(await digest(bytes));
      const wantCompression = request.headers.get('X-Bmux-Compression') === 'gzip-v1';
      const packedFull = wantCompression ? await compressed(bytes) : null;
      if (delta) delta.compressed = wantCompression ? await compressed(delta.bytes) : null;
      return await this.serialized(async () => {
        route(request, this.env, this.local);
        await authorize(request, this.env);
        await this.expire(); // Body/hash/auth time cannot extend an expired lease.
        const m = this.meta;
        if (m.epoch !== admitted.epoch || m.fence !== admitted.fence || m.state === 'paused') reject(409);
        if (BigInt(revision) < BigInt(m.revision) || (revision === m.revision && hash !== m.frame_hash)) reject(409);
        await this.commit({ ...m, state: 'live', revision, frame_hash: hash, lease_expires_at: this.now() + LEASE_MS }, bytes);
        this.delta = delta; this.identity = identity; this.compressedFull = packedFull;
        return response(204);
      });
    } finally { this.uploads--; }
  }
}

export class LiveChannel {
  constructor(ctx, env) { this.handler = new ChannelHandler(ctx.storage, env); }
  fetch(request) { return this.handler.fetch(request); }
}
export default {
  async fetch(request, env) {
    try {
      route(request, env);
      const id = env.LIVE_CHANNELS.idFromName(configured(env));
      return await env.LIVE_CHANNELS.get(id).fetch(request);
    } catch (e) { return response(e instanceof Rejection ? e.status : 503, { error: 'request rejected' }); }
  },
};
