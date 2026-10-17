import test from 'node:test';
import assert from 'node:assert/strict';
import { mountLiveViewer, revision, validateStatus } from '../browser/live-viewer.js';

const epoch = '11111111-1111-4111-8111-111111111111';
const bytes = () => new Uint8Array([66, 77, 87, 80, 4, 0]).buffer;
async function fixture(options = {}) {
  const frames = new Map(), timers = new Map(), sockets = [];
  let id = 0, paints = 0, clears = 0, identity;
  class Socket {
    constructor(url) { this.url = url; this.readyState = 1; this.sent = []; sockets.push(this); }
    send(value) { this.sent.push(JSON.parse(value)); }
    close() { this.readyState = 3; this.onclose?.(); }
    state(state = 'live', fence = '1') {
      this.onmessage({ data: JSON.stringify({ type: 'state', state, epoch, fence, lease_ms: 15000, lease_remaining_ms: 15000 }) });
    }
    frame() { this.onmessage({ data: bytes() }); }
  }
  const viewer = {
    clear() { clears++; identity = null; },
    async present(data, current) {
      paints++;
      if (options.present) await options.present();
      if (!current()) return false;
      identity = { generation: 'scene', revision: String(paints) };
      return true;
    },
    get identity() { return identity; },
    dispose() {},
  };
  const handle = await mountLiveViewer({
    container: { ownerDocument: { baseURI: 'https://example.com/' } }, status: {}, assetBase: '/',
    viewerFactory: async () => viewer, WebSocketImpl: Socket,
    requestFrame: callback => { frames.set(++id, callback); return id; },
    cancelFrame: key => frames.delete(key),
    setTimer: (callback, delay) => { timers.set(++id, { callback, delay }); return id; },
    clearTimer: key => timers.delete(key), ...options,
  });
  return { handle, sockets, frames, timers, get paints() { return paints; }, get clears() { return clears; },
    async raf(now) { const work = [...frames.values()]; frames.clear(); await Promise.all(work.map(fn => fn(now))); },
    timer() { const [key, timer] = [...timers.entries()].sort((a, b) => a[1].delay - b[1].delay)[0]; timers.delete(key); timer.callback(); },
  };
}

test('strict authority and lossless u64 parsing', () => {
  assert.equal(revision('18446744073709551615'), '18446744073709551615');
  for (const value of ['01', '-1', '18446744073709551616', 1]) assert.throws(() => revision(value));
  assert.throws(() => validateStatus({ type: 'state', state: 'live', epoch: null, fence: '1' }));
});
test('idle schedules no RAF, frame traffic or ACKs, only a fail-closed lease deadline', async () => {
  const f = await fixture(); f.sockets[0].state();
  assert.equal(f.frames.size, 0); assert.equal(f.timers.size, 1); assert.equal(f.paints, 0);
  assert.deepEqual(f.sockets[0].sent, []); f.handle.dispose();
});
test('tiny update schedules one RAF and only acknowledges admitted identity', async () => {
  const f = await fixture(); const socket = f.sockets[0]; socket.state(); socket.frame();
  assert.equal(f.frames.size, 1); assert.deepEqual(socket.sent, []);
  await f.raf(0);
  assert.deepEqual(socket.sent, [{ type: 'ack', generation: 'scene', revision: '1' }]);
  assert.equal(f.frames.size, 0); assert.equal(f.timers.size, 1); f.handle.dispose();
});
test('FPS cap waits only while dirty, then returns to idle', async () => {
  const f = await fixture({ maxFps: 30 }); const s = f.sockets[0]; s.state(); s.frame(); await f.raf(0);
  s.frame(); await f.raf(1); assert.equal(f.paints, 1); assert.equal(f.timers.size, 2);
  f.timer(); await f.raf(34); assert.equal(f.paints, 2); assert.equal(f.timers.size, 1);
  f.handle.dispose();
});
test('uncredited burst disconnects instead of silently dropping a delta', async () => {
  const f = await fixture(); const s = f.sockets[0]; s.state(); s.frame(); s.frame();
  assert.equal(s.readyState, 3); assert.equal(f.frames.size, 0); assert.equal(f.paints, 0); f.handle.dispose();
});
test('pause invalidates asynchronous paint and cannot ACK old generation', async () => {
  let resolve; const blocked = new Promise(done => { resolve = done; });
  const f = await fixture({ present: () => blocked }); const s = f.sockets[0]; s.state(); s.frame();
  const painting = f.raf(0); s.state('paused', '2'); resolve(); await painting;
  assert.deepEqual(s.sent, []); assert.equal(f.frames.size, 0); f.handle.dispose();
});
test('baseline failure requests full repair, next admitted frame ACKs', async () => {
  let fail = true;
  const f = await fixture({ present: () => { if (fail) { fail = false; throw new Error('base'); } } });
  const s = f.sockets[0]; s.state(); s.frame(); await f.raf(0);
  assert.deepEqual(s.sent, [{ type: 'repair' }]); s.frame(); await f.raf(20);
  assert.equal(s.sent.at(-1).type, 'ack'); f.handle.dispose();
});
test('disconnect clears baseline and reconnect has no stale RAF or ACK', async () => {
  const f = await fixture(); const s = f.sockets[0]; s.state(); s.frame(); s.close();
  assert.equal(f.frames.size, 0); f.timer(); assert.equal(f.sockets.length, 2);
  const next = f.sockets[1]; next.state('starting', '2'); next.frame(); await f.raf(0);
  assert.equal(next.sent[0].type, 'ack'); assert.deepEqual(s.sent, []); f.handle.dispose();
  assert.equal(f.frames.size, 0); assert.equal(f.timers.size, 0);
});
test('lease uses remaining authority, expires closed, and renewal does not draw', async () => {
  const f = await fixture(); const s = f.sockets[0];
  const state = { type: 'state', state: 'live', epoch, fence: '1', lease_ms: 15000, lease_remaining_ms: 200 };
  s.onmessage({ data: JSON.stringify(state) });
  assert.equal([...f.timers.values()][0].delay, 200);
  s.frame(); await f.raf(0);
  s.onmessage({ data: JSON.stringify({ ...state, lease_remaining_ms: 100 }) });
  assert.equal(f.paints, 1); assert.equal(f.frames.size, 0);
  assert.equal([...f.timers.values()][0].delay, 100);
  f.timer(); assert.equal(s.readyState, 3); assert.ok(f.clears > 1); f.handle.dispose();
});

test('invalid FPS rejected', async () => {
  for (const maxFps of [0, 61, NaN, Infinity]) await assert.rejects(fixture({ maxFps }));
});
