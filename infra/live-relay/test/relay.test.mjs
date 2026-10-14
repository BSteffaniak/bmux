import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, rm, readFile, writeFile, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import worker, { ChannelHandler, LiveChannel, MAX_FRAME_BYTES, validateSnapshot } from '../src/worker.js';
import { createLocalServer, LocalStorage } from '../local.mjs';
const key = 'test-only-publisher-key';
const env = { CHANNEL: 'main', PUBLISHER_KEY_SHA256: createHash('sha256').update(key).digest('hex') };
const publisher = '11111111-1111-4111-8111-111111111111';
class MemoryStorage {
  async get() { return this.value === undefined ? undefined : structuredClone(this.value); }
  async put(_key, value) { this.value = structuredClone(value); }
}
// Explicit positional envelope fixture. Metadata is opaque to admission, not
// claimed as a Rust semantic fixture; cross-language validation uses Rust output.
function fixture({ revision = 1, delta = false, resources = false } = {}) {
  const out = [66, 77, 87, 80, 2];
  const uint = n => { n = BigInt(n); do { let b = Number(n & 127n); n >>= 7n; out.push(b | (n ? 128 : 0)); } while (n); };
  const blob = bytes => { uint(bytes.length); out.push(...bytes); };
  const text = s => blob(new TextEncoder().encode(s));
  text('relay-fixture'); uint(revision); out.push(delta ? 1 : 0);
  if (delta) { text('relay-fixture'); uint(0); }
  [0, 0, 1, 1].forEach(uint); // Rect
  uint(1); out.push(0, 0); uint(0); // One default style
  uint(1); text(' '); // One glyph
  uint(1); uint(0); uint(1); uint(0); uint(0); uint(0); out.push(1, 0); uint(1); // One full row
  out.push(1); blob([0]); out.push(1); uint(resources ? 1 : 0);
  const requirement = () => { text('a'.repeat(64)); text('application/octet-stream'); uint(1); uint(1); out.push(1); uint(1); };
  if (resources) { text('image'); requirement(); [0, 0, 1, 1].forEach(uint); uint(1); out.push(0); [0, 0, 1, 1].forEach(uint); uint(0); }
  uint(resources ? 1 : 0); if (resources) { requirement(); blob([1, 2, 3, 4]); }
  return Uint8Array.from(out);
}
function request(action, { epoch, revision, body, auth = true, method, headers = {}, host = 'https://relay.invalid', query = '' } = {}) {
  const publisherRoute = action.startsWith('publisher/');
  const h = { ...(auth && publisherRoute ? { Authorization: `Bearer ${key}` } : {}), ...headers };
  if (epoch) h['X-Bmux-Epoch'] = epoch;
  if (revision) h['X-Bmux-Revision'] = revision;
  if (action === 'publisher/start' && body === undefined) { body = JSON.stringify({ protocol: 1, renderer: 'bmwp-2', publisher_id: publisher }); h['Content-Type'] = 'application/json'; }
  if (action === 'publisher/frame') h['Content-Type'] ??= 'application/octet-stream';
  return new Request(`${host}/api/live/main/${action}${query}`, { method: method ?? (action === 'publisher/frame' ? 'PUT' : publisherRoute ? 'POST' : 'GET'), headers: h, body, ...(body instanceof ReadableStream ? { duplex: 'half' } : {}) });
}
async function start(handler) { const r = await handler.fetch(request('publisher/start')); assert.equal(r.status, 200); return (await r.json()).epoch; }
async function status(handler) { return (await handler.fetch(request('status'))).json(); }
async function upload(handler, epoch, revision = '1', body = fixture()) { return handler.fetch(request('publisher/frame', { epoch, revision, body })); }

test('full lifecycle, duplicate/conflict revisions, paused retries and output-only routes', async () => {
  const handler = new ChannelHandler(new MemoryStorage(), env);
  const epoch = await start(handler);
  assert.equal((await status(handler)).state, 'starting');
  assert.equal((await upload(handler, epoch)).status, 204);
  assert.equal((await upload(handler, epoch)).status, 204);
  assert.equal((await upload(handler, epoch, '1', fixture({ revision: 2 }))).status, 409);
  const frame = await handler.fetch(request('presentation', { query: `?epoch=${epoch}` }));
  assert.equal(frame.status, 200); assert.equal(frame.headers.get('X-Snapshot-Revision'), '1');
  assert.deepEqual(new Uint8Array(await frame.arrayBuffer()), fixture());
  assert.equal((await handler.fetch(request('presentation', { query: `?epoch=${epoch}&after=1` }))).status, 204);
  assert.equal((await handler.fetch(request('presentation', { query: '?epoch=wrong' }))).status, 409);
  assert.equal((await handler.fetch(request('publisher/pause', { epoch }))).status, 204);
  assert.equal(await start(handler), epoch); assert.equal((await status(handler)).state, 'paused');
  assert.equal((await upload(handler, epoch, '2')).status, 409);
  assert.equal((await handler.fetch(request('publisher/heartbeat', { epoch }))).status, 204);
  assert.equal((await status(handler)).state, 'paused');
  assert.equal((await handler.fetch(request('presentation', { query: `?epoch=${epoch}` }))).status, 503);
  assert.equal((await handler.fetch(request('publisher/resume', { epoch }))).status, 204);
  assert.equal((await status(handler)).state, 'starting');
  assert.equal((await upload(handler, epoch, '2')).status, 204);
  assert.equal((await upload(handler, epoch, '1')).status, 409);
  assert.equal((await handler.fetch(request('publisher/stop', { epoch }))).status, 204);
  const replacement = await start(handler); assert.notEqual(replacement, epoch);
  assert.equal((await handler.fetch(request('publisher/stop', { epoch }))).status, 204);
  assert.equal((await status(handler)).epoch, replacement);
  assert.equal((await upload(handler, epoch, '3')).status, 409);
  assert.equal((await handler.fetch(request('view'))).status, 404);
});
test('durable restart, paused/stop retention, exact retries repair volatile frames', async () => {
  const storage = new MemoryStorage();
  let handler = new ChannelHandler(storage, env);
  const epoch = await start(handler); await upload(handler, epoch);
  handler = new ChannelHandler(storage, env);
  assert.equal((await status(handler)).state, 'starting');
  assert.equal((await upload(handler, epoch)).status, 204);
  await handler.fetch(request('publisher/pause', { epoch }));
  handler = new ChannelHandler(storage, env);
  assert.equal((await status(handler)).state, 'paused');
  await handler.fetch(request('publisher/stop', { epoch }));
  handler = new ChannelHandler(storage, env);
  assert.equal((await status(handler)).state, 'offline');
  assert.equal((await handler.fetch(request('publisher/stop', { epoch }))).status, 204);
  assert.ok(JSON.stringify(storage.value).length < 1024);
});
test('lease expires on every access; expired ownership cannot be revived', async () => {
  let now = 1000;
  const handler = new ChannelHandler(new MemoryStorage(), env, { now: () => now });
  const epoch = await start(handler); await upload(handler, epoch);
  now += 15000;
  assert.equal((await handler.fetch(request('publisher/heartbeat', { epoch }))).status, 409);
  assert.equal((await status(handler)).state, 'offline');
  assert.notEqual(await start(handler), epoch);
});
test('auth, channel allowlist, unsupported versions, headers and complete envelope rejection', async () => {
  const handler = new ChannelHandler(new MemoryStorage(), env);
  assert.equal((await handler.fetch(request('publisher/start', { auth: false }))).status, 401);
  assert.equal((await handler.fetch(request('publisher/start', { headers: { Authorization: 'Bearer wrong' } }))).status, 401);
  assert.equal((await handler.fetch(new Request('https://relay.invalid/api/live/deleted/status'))).status, 404);
  assert.equal((await handler.fetch(request('publisher/start', { body: JSON.stringify({ protocol: 2 }), headers: { 'Content-Type': 'application/json' } }))).status, 415);
  const epoch = await start(handler);
  for (const bad of [fixture().slice(0, -1), Uint8Array.of(66, 77, 87, 80, 2), Uint8Array.from([...fixture(), 0]), fixture({ delta: true })]) assert.equal((await upload(handler, epoch, '1', bad)).status, 400);
  assert.equal((await upload(handler, epoch, '1', Uint8Array.of(66, 77, 87, 80, 3))).status, 415);
  for (const rev of ['0', '-1', '01', '18446744073709551616']) assert.equal((await upload(handler, epoch, rev)).status, 400);
  validateSnapshot(fixture({ resources: true }));
  assert.equal((await upload(handler, epoch, '1', fixture(),)).status, 204);
  const r = await handler.fetch(request('status'));
  assert.equal(r.headers.get('Cache-Control'), 'no-store'); assert.equal(r.headers.get('Access-Control-Allow-Origin'), null);
  assert.equal((await handler.fetch(request('publisher/frame', { epoch, revision: '2', body: fixture(), headers: { 'Content-Type': 'text/plain' } }))).status, 415);
});
test('pause/resume and stop win against body-stream race; upload slots bounded', async () => {
  const handler = new ChannelHandler(new MemoryStorage(), env);
  const epoch = await start(handler);
  let controller;
  const stream = new ReadableStream({ start(c) { controller = c; } });
  const pending = upload(handler, epoch, '1', stream);
  while (!handler.uploads) await new Promise(r => setTimeout(r, 1));
  // Wait for upload preflight to finish before pausing while its body is stalled.
  await handler.tail;
  assert.equal((await upload(handler, epoch, '2')).status, 429);
  await handler.fetch(request('publisher/pause', { epoch }));
  await handler.fetch(request('publisher/resume', { epoch }));
  controller.enqueue(fixture()); controller.close();
  assert.equal((await pending).status, 409); assert.equal((await status(handler)).state, 'starting');
  let control;
  const pendingStop = upload(handler, epoch, '1', new ReadableStream({ start(c) { control = c; } }));
  while (!handler.uploads) await new Promise(r => setTimeout(r, 1));
  await handler.tail;
  await handler.fetch(request('publisher/stop', { epoch }));
  const next = await start(handler);
  control.enqueue(fixture()); control.close();
  assert.equal((await pendingStop).status, 409); assert.equal((await status(handler)).epoch, next);
});
test('actual streaming overflow, dishonest length and body deadline', async () => {
  const handler = new ChannelHandler(new MemoryStorage(), env, { bodyTimeoutMs: 30 });
  const epoch = await start(handler);
  let chunks = 0;
  const overflow = new ReadableStream({ pull(c) { if (chunks++ < 9) c.enqueue(new Uint8Array(1024 * 1024)); else c.close(); } });
  assert.equal((await upload(handler, epoch, '1', overflow)).status, 413);
  assert.equal((await handler.fetch(request('publisher/frame', { epoch, revision: '1', body: fixture(), headers: { 'Content-Length': '1' } }))).status, 400);
  assert.equal((await upload(handler, epoch, '1', new ReadableStream())).status, 408);
  assert.equal(handler.uploads, 0); assert.equal((await status(handler)).state, 'starting');
  assert.equal(MAX_FRAME_BYTES, 8388608);
});
test('concurrent start ownership and durable write acknowledgment', async () => {
  const storage = new MemoryStorage(); const handler = new ChannelHandler(storage, env);
  const one = request('publisher/start');
  const two = request('publisher/start', { body: JSON.stringify({ protocol: 1, renderer: 'bmwp-2', publisher_id: '22222222-2222-4222-8222-222222222222' }), headers: { 'Content-Type': 'application/json' } });
  const statuses = (await Promise.all([handler.fetch(one), handler.fetch(two)])).map(r => r.status).sort();
  assert.deepEqual(statuses, [200, 409]);
  const epoch = (await status(handler)).epoch;
  storage.put = async () => { throw new Error('private storage failure'); };
  assert.equal((await upload(handler, epoch)).status, 503);
  assert.equal((await status(handler)).state, 'starting');
});
test('production exports use DO handler, reject HTTP, unknown storage compatibility', async () => {
  const ctx = { storage: new MemoryStorage() }; const object = new LiveChannel(ctx, env);
  let name;
  const bindings = { ...env, LIVE_CHANNELS: { idFromName(n) { name = n; return n; }, get() { return object; } } };
  assert.equal((await worker.fetch(request('health'), bindings)).status, 200); assert.equal(name, 'main');
  assert.equal((await worker.fetch(request('health', { host: 'http://relay.invalid' }), bindings)).status, 400);
  const storage = new MemoryStorage(); storage.value = { version: 99 };
  const handler = new ChannelHandler(storage, env);
  assert.equal((await handler.fetch(request('status'))).status, 503);
});
test('elapsed lease fences a body validated after ownership expires', async () => {
  let now = 1000;
  const handler = new ChannelHandler(new MemoryStorage(), env, { now: () => now });
  const epoch = await start(handler);
  let control;
  const pending = upload(handler, epoch, '1', new ReadableStream({ start(c) { control = c; } }));
  while (!handler.uploads) await new Promise(r => setTimeout(r, 1));
  await handler.tail; now += 15000;
  control.enqueue(fixture()); control.close();
  assert.equal((await pending).status, 409);
  assert.equal((await status(handler)).state, 'offline');
});
test('pending viewer streams are backpressured, bounded and invalidated by pause/stop', async () => {
  const handler = new ChannelHandler(new MemoryStorage(), env);
  const epoch = await start(handler); await upload(handler, epoch);
  const deliveries = [];
  for (let i = 0; i < 4; i++) deliveries.push(await handler.fetch(request('presentation', { query: `?epoch=${epoch}` })));
  assert.equal(handler.viewers, 4);
  assert.equal((await handler.fetch(request('presentation', { query: `?epoch=${epoch}` }))).status, 429);
  await handler.fetch(request('publisher/pause', { epoch }));
  for (const delivery of deliveries) await assert.rejects(delivery.arrayBuffer());
  assert.equal(handler.viewers, 0);
  await handler.fetch(request('publisher/resume', { epoch }));
  await upload(handler, epoch, '2');
  const stale = await handler.fetch(request('presentation', { query: `?epoch=${epoch}` }));
  await handler.fetch(request('publisher/stop', { epoch }));
  await assert.rejects(stale.arrayBuffer()); assert.equal(handler.viewers, 0);
});
test('admission interleavings do not undo pause or allow stale stop to replace owner', async () => {
  const handler = new ChannelHandler(new MemoryStorage(), env);
  const epoch = await start(handler); await upload(handler, epoch);
  let control;
  const body = new ReadableStream({ start(c) { control = c; } });
  const delayedStart = handler.fetch(request('publisher/start', { body, headers: { 'Content-Type': 'application/json' } }));
  await handler.fetch(request('publisher/pause', { epoch }));
  control.enqueue(new TextEncoder().encode(JSON.stringify({ protocol: 1, renderer: 'bmwp-2', publisher_id: publisher }))); control.close();
  assert.equal((await delayedStart).status, 200); assert.equal((await status(handler)).state, 'paused');
  const pausedDelivery = await handler.fetch(request('presentation', { query: `?epoch=${epoch}` }));
  assert.equal(pausedDelivery.status, 503);
});
test('same-origin static allowlist serves viewer dist without arbitrary disk exposure', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'bmux-relay-static-'));
  let running;
  try {
    await writeFile(join(dir, 'index.html'), '<!doctype html><title>isolated viewer</title>');
    await writeFile(join(dir, 'viewer.js'), 'export const test = true;');
    await writeFile(join(dir, 'secret.txt'), 'not an allowed type');
    running = await createLocalServer({ env, staticDir: dir });
    assert.equal((await fetch(`${running.url}/`)).status, 200);
    const js = await fetch(`${running.url}/viewer.js`);
    assert.equal(js.status, 200); assert.match(js.headers.get('Content-Type'), /javascript/);
    assert.equal((await fetch(`${running.url}/secret.txt`)).status, 404);
    assert.equal((await fetch(`${running.url}/api/live/deleted/status`)).status, 404);
    assert.equal((await fetch(`${running.url}/%2e%2e/secret.txt`)).status, 404);
    await new Promise(r => { running.server.close(r); running.server.closeAllConnections(); }); running = null;
    await symlink(join(dir, 'viewer.js'), join(dir, 'escape.js'));
    await assert.rejects(createLocalServer({ env, staticDir: dir }), /unsafe/);
    await assert.rejects(createLocalServer({ env, host: '0.0.0.0' }), /loopback/);
  } finally {
    if (running) await new Promise(r => { running.server.close(r); running.server.closeAllConnections(); });
    await rm(dir, { recursive: true });
  }
});
test('real isolated Node HTTP publish/status/presentation and persistent restart', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'bmux-relay-test-'));
  let running;
  try {
    const storage = new LocalStorage(join(dir, 'metadata.json'));
    running = await createLocalServer({ env, storage });
    const call = (action, opts = {}) => fetch(request(action, { ...opts, host: running.url }));
    const epoch = (await (await call('publisher/start')).json()).epoch;
    assert.equal((await call('publisher/frame', { epoch, revision: '1', body: fixture() })).status, 204);
    assert.equal((await (await call('status')).json()).state, 'live');
    assert.deepEqual(new Uint8Array(await (await call('presentation', { query: `?epoch=${epoch}` })).arrayBuffer()), fixture());
    await call('publisher/pause', { epoch });
    await new Promise(r => { running.server.close(r); running.server.closeAllConnections(); });
    running = await createLocalServer({ env, storage: new LocalStorage(join(dir, 'metadata.json')) });
    assert.equal((await (await call('status')).json()).state, 'paused');
    assert.equal((await call('publisher/stop', { epoch })).status, 204);
  } finally {
    if (running) await new Promise(r => { running.server.close(r); running.server.closeAllConnections(); });
    await rm(dir, { recursive: true });
  }
});
