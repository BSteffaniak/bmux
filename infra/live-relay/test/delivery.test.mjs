import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { gzipSync, gunzipSync } from 'node:zlib';
import { ChannelHandler, validateSnapshot } from '../src/worker.js';

const key = 'isolated-delta-admission-test';
const env = { CHANNEL: 'main', DELTA_BUNDLES: 'fanout', PUBLISHER_KEY_SHA256: createHash('sha256').update(key).digest('hex') };
const publisher = '11111111-1111-4111-8111-111111111111';
const capability = { 'X-Bmux-Capabilities': 'bmwp-delta-v1' };
class Storage {
  async get() { return this.value && structuredClone(this.value); }
  async put(_key, value) { this.value = structuredClone(value); }
}
// Positional admission fixture only: opaque metadata is not a Rust semantic
// fixture. Real encode_delta coverage belongs in the cross-language test below.
function frame({ generation = 'one', revision = 1, base = null, padding = 64 } = {}) {
  const out = [66, 77, 87, 80, 3];
  const uint = value => { let n = BigInt(value); do { const b = Number(n & 127n); n >>= 7n; out.push(b | (n ? 128 : 0)); } while (n); };
  const blob = value => { uint(value.length); out.push(...value); };
  const text = value => blob(new TextEncoder().encode(value));
  text(generation); uint(revision); out.push(base ? 1 : 0);
  if (base) { text(base.generation); uint(base.revision); }
  [0, 0, 1, 1].forEach(uint);
  if (base) { uint(0); uint(0); uint(0); out.push(0, 0); uint(0); }
  else {
    uint(1); out.push(0, 0); uint(0);
    uint(1); text(' ');
    uint(1); uint(0); uint(1); uint(0); uint(0); uint(0); out.push(1, 0); uint(1);
    out.push(1); blob(new Uint8Array(padding)); out.push(1); uint(0); uint(0);
  }
  return Uint8Array.from(out);
}
function bundle(full, delta) {
  const body = new Uint8Array(4 + full.length + delta.length);
  new DataView(body.buffer).setUint32(0, full.length, true);
  body.set(full, 4); body.set(delta, 4 + full.length); return body;
}
function request(path, { epoch, revision, body, headers = {}, method } = {}) {
  return new Request(`https://relay.test/api/live/main/${path}`, {
    method: method ?? (path.startsWith('publisher/') ? (body && path.endsWith('/frame') ? 'PUT' : 'POST') : 'GET'),
    headers: { ...(path.startsWith('publisher/') ? { Authorization: `Bearer ${key}` } : {}),
      ...(epoch ? { 'X-Bmux-Epoch': epoch } : {}), ...(revision ? { 'X-Bmux-Revision': revision } : {}),
      ...(body ? { 'Content-Type': 'application/octet-stream' } : {}), ...headers }, body,
  });
}
async function start(handler, negotiated = true) {
  const result = await handler.fetch(request('publisher/start', { body: JSON.stringify({ protocol: 1, renderer: 'bmwp-3', publisher_id: publisher }),
    headers: { 'Content-Type': 'application/json', ...(negotiated ? capability : {}) } }));
  assert.equal(result.status, 200);
  assert.equal(result.headers.get('X-Bmux-Capabilities'), negotiated ? 'bmwp-delta-v1' : null);
  return (await result.json()).epoch;
}
async function upload(handler, epoch, revision, body, headers = {}) {
  return handler.fetch(request('publisher/frame', { epoch, revision, body, headers }));
}
async function read(handler, epoch, suffix = '', headers = capability) {
  const result = await handler.fetch(request(`presentation?epoch=${epoch}${suffix}`, { headers }));
  const body = new Uint8Array(await result.arrayBuffer()); return { result, body };
}
async function pair() {
  const handler = new ChannelHandler(new Storage(), env);
  const epoch = await start(handler);
  const first = frame(); const full = frame({ revision: 2 });
  const delta = frame({ revision: 2, base: { generation: 'one', revision: 1 } });
  assert.equal((await upload(handler, epoch, '1', first)).status, 204);
  const body = bundle(full, delta);
  assert.equal((await upload(handler, epoch, '2', body, { ...capability, 'X-Bmux-Base-Revision': '1' })).status, 204);
  return { handler, epoch, full, delta, body };
}

test('exact epoch + relay base + BMWP generation/revision selects only smaller delta; legacy gets full', async () => {
  const { handler, epoch, full, delta } = await pair();
  assert.ok(delta.length < full.length);
  const selected = await read(handler, epoch, '&after=1&generation=one&base=1');
  assert.equal(selected.result.headers.get('X-Bmux-Frame'), 'delta');
  assert.equal(selected.result.headers.get('X-Bmux-Base-Revision'), '1');
  assert.equal(selected.result.headers.get('X-Bmux-Base-Frame-Revision'), '1');
  assert.deepEqual(selected.body, delta);
  for (const suffix of ['', '&after=1', '&after=1&generation=wrong&base=1', '&after=1&generation=one&base=0', '&after=0&generation=one&base=1']) {
    const repair = await read(handler, epoch, suffix);
    assert.equal(repair.result.headers.get('X-Bmux-Frame'), 'full'); assert.deepEqual(repair.body, full);
  }
  const legacy = await read(handler, epoch, '&after=1', {});
  assert.equal(legacy.result.headers.get('X-Bmux-Capabilities'), null);
  assert.deepEqual(legacy.body, full);
  assert.equal((await read(handler, '22222222-2222-4222-8222-222222222222')).result.status, 409);
  assert.equal((await read(handler, epoch, '&after=2&generation=one&base=2')).result.status, 204);
  assert.deepEqual((await read(handler, epoch, '&after=2&generation=wrong&base=2')).body, full);
  assert.ok(handler.counters.bundle_overhead_bytes > 0);
  assert.equal(handler.counters.upload_wire_bytes - handler.counters.upload_full_bytes, handler.counters.bundle_overhead_bytes);
  assert.ok(handler.counters.delta > 0); assert.ok(handler.counters.repair > 0);
});

test('wrong upload base/generation/target rejects without replacing full repair', async () => {
  const { handler, epoch, full } = await pair();
  for (const [base, delta] of [
    ['1', frame({ revision: 3, base: { generation: 'one', revision: 1 } })],
    ['2', frame({ generation: 'other', revision: 3, base: { generation: 'other', revision: 2 } })],
    ['2', frame({ revision: 4, base: { generation: 'one', revision: 2 } })],
  ]) {
    const body = bundle(frame({ revision: 3 }), delta);
    assert.equal((await upload(handler, epoch, '3', body, { ...capability, 'X-Bmux-Base-Revision': base })).status, 409);
    assert.deepEqual((await read(handler, epoch)).body, full);
  }
  assert.throws(() => validateSnapshot(frame({ revision: 3, base: { generation: 'one', revision: 2 } })));
});

test('lost upload reply retry is idempotent and falls back to full repair', async () => {
  const { handler, epoch, full, body } = await pair();
  assert.equal((await upload(handler, epoch, '2', body, { ...capability, 'X-Bmux-Base-Revision': '1' })).status, 204);
  const repair = await read(handler, epoch, '&after=1&generation=one&base=1');
  assert.equal(repair.result.headers.get('X-Bmux-Frame'), 'full'); assert.deepEqual(repair.body, full);
});

test('source switch pause/resume and durable reload require full before deltas', async () => {
  const { handler, epoch } = await pair();
  assert.equal((await handler.fetch(request('publisher/pause', { epoch }))).status, 204);
  assert.equal((await handler.fetch(request('publisher/resume', { epoch }))).status, 204);
  assert.equal((await read(handler, epoch)).result.status, 503);
  const full = frame({ generation: 'replacement', revision: 1 });
  assert.equal((await upload(handler, epoch, '3', full)).status, 204);
  assert.deepEqual((await read(handler, epoch, '&after=2&generation=one&base=2')).body, full);
  const reloaded = new ChannelHandler(handler.storage, env);
  assert.equal((await read(reloaded, epoch)).result.status, 503);
  assert.equal((await upload(reloaded, epoch, '4', full)).status, 204);
  assert.deepEqual((await read(reloaded, epoch)).body, full);
});

test('default deployment refuses bundle opt-in and preserves full legacy admission', async () => {
  const handler = new ChannelHandler(new Storage(), { ...env, DELTA_BUNDLES: undefined });
  const result = await handler.fetch(request('publisher/start', { body: JSON.stringify({ protocol: 1, renderer: 'bmwp-3', publisher_id: publisher }),
    headers: { 'Content-Type': 'application/json', ...capability } }));
  assert.equal(result.status, 200); assert.equal(result.headers.get('X-Bmux-Capabilities'), null);
  const { epoch } = await result.json(); const full = frame();
  assert.equal((await upload(handler, epoch, '1', full)).status, 204);
  const delta = frame({ revision: 2, base: { generation: 'one', revision: 1 } });
  assert.equal((await upload(handler, epoch, '2', bundle(frame({ revision: 2 }), delta), { ...capability, 'X-Bmux-Base-Revision': '1' })).status, 415);
  assert.deepEqual((await read(handler, epoch, '', {})).body, full);
});

test('native gzip is explicit, smaller, bounded, and legacy payload stays unchanged', async () => {
  const handler = new ChannelHandler(new Storage(), env); const epoch = await start(handler, false);
  const full = frame({ padding: 4096 });
  assert.equal((await upload(handler, epoch, '1', full, { 'X-Bmux-Compression': 'gzip-v1' })).status, 204);
  const packed = await read(handler, epoch, '', { ...capability, 'X-Bmux-Compression': 'gzip-v1' });
  assert.equal(packed.result.headers.get('X-Bmux-Compression'), 'gzip-v1');
  assert.ok(packed.body.length < full.length); assert.deepEqual(new Uint8Array(gunzipSync(packed.body)), full);
  assert.deepEqual((await read(handler, epoch, '', {})).body, full);
  assert.ok(gzipSync(full).length < full.length);
});
