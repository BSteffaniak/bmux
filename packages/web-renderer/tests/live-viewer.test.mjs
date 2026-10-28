import test from 'node:test';
import assert from 'node:assert/strict';
import { mountLiveViewer, revision, validateStatus } from '../browser/live-viewer.js';

const epoch = '11111111-1111-4111-8111-111111111111';
const bytes = () => new Uint8Array([66, 77, 87, 80, 5, 0]).buffer;
async function fixture(options = {}) {
  const frames = new Map(), timers = new Map(), sockets = [];
  let id = 0, paints = 0, clears = 0, identity, clock = 0;
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
    async present(data, current, decoded) {
      paints++;
      if (options.present) await options.present();
      if (options.paintDuration) clock += options.paintDuration(paints);
      if (!current()) return false;
      decoded();
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
    clearTimer: key => timers.delete(key), now: () => clock, ...options,
  });
  return { handle, sockets, frames, timers, advance(ms) { clock += ms; }, get paints() { return paints; }, get clears() { return clears; },
    async raf(now, actual = now) { clock = actual; const work = [...frames.values()]; frames.clear(); await Promise.all(work.map(fn => fn(now))); },
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
test('obsolete and unknown BMWP versions request repair without paint or ACK', async () => {
  for (const version of [2, 4, 6]) {
    const f = await fixture(); const s = f.sockets[0]; s.state();
    const frame = new Uint8Array(bytes()); frame[4] = version;
    s.onmessage({ data: frame.buffer });
    await f.raf(0);
    assert.equal(f.paints, 0);
    assert.deepEqual(s.sent, [{ type: 'repair' }]);
    f.handle.dispose();
  }
});

test('FPS cap waits only while dirty, then returns to idle', async () => {
  const f = await fixture({ maxFps: 30 }); const s = f.sockets[0]; s.state(); s.frame(); await f.raf(0);
  s.frame(); await f.raf(1); assert.equal(f.paints, 1); assert.equal(f.timers.size, 2);
  f.timer(); await f.raf(34); assert.equal(f.paints, 2); assert.equal(f.timers.size, 1);
  f.handle.dispose();
});
test('slow asynchronous paint cannot compress the next admission interval', async () => {
  const f = await fixture({ paintDuration: paint => paint === 1 ? 100 : 0 });
  const s = f.sockets[0]; s.state(); s.frame(); await f.raf(0);
  assert.equal(s.sent.length, 1); // First admission occurs at 100, not RAF timestamp 0.
  s.frame(); await f.raf(100);
  assert.equal(f.paints, 1, 'must wait after completion, not after RAF start');
  assert.equal(s.sent.length, 1, 'pending frame must not ACK before admission');
  f.timer(); await f.raf(116);
  assert.equal(f.paints, 1, 'early timer/RAF must not bypass the 60 FPS ceiling');
  f.timer(); await f.raf(117);
  assert.equal(f.paints, 2); assert.equal(s.sent.length, 2);
  assert.equal(f.timers.size, 1); // Lease only: no idle frame timer.
  f.handle.dispose();
});
test('RAF timeline timestamps cannot stand in for admission wall time', async () => {
  const f = await fixture(); const s = f.sockets[0]; s.state(); s.frame();
  await f.raf(0, 100); // Main-thread work delayed this RAF callback by 100ms.
  s.frame(); await f.raf(117, 117);
  assert.equal(f.paints, 2);
  s.frame(); await f.raf(134, 130);
  assert.equal(f.paints, 2, 'admissions must be separated even when RAF timestamps are not');
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

test('diagnostics separate retained idle from known pending stall without false stale alerts', async () => {
  const f = await fixture(); const s = f.sockets[0]; s.state();
  assert.equal(f.handle.diagnostics.progress, 'idle');
  assert.equal(f.handle.diagnostics.agesMs.submitted, null);
  s.frame(); f.advance(6000);
  assert.equal(f.handle.diagnostics.progress, 'pending-stalled');
  await f.raf(6000);
  const d = f.handle.diagnostics;
  assert.equal(d.progress, 'idle'); assert.equal(d.retained, true);
  assert.equal(d.counts.decoded, 1); assert.equal(d.counts.submitted, 1); assert.equal(d.counts.ackSent, 1);
  assert.deepEqual(d.submitted, { generation: 1, revision: '1' });
  f.advance(60000);
  assert.equal(f.handle.diagnostics.progress, 'idle');
  assert.equal(f.handle.diagnostics.agesMs.submitted, 60000);
  f.handle.dispose(); assert.equal(f.handle.diagnostics.progress, 'disposed');
});

test('stalled decoder and obsolete completion do not fabricate submission or ACK progress', async () => {
  let finish;
  const f = await fixture({ present: () => new Promise(resolve => { finish = resolve; }) });
  const s = f.sockets[0]; s.state(); s.frame(); const painting = f.raf(0);
  f.advance(6000); assert.equal(f.handle.diagnostics.progress, 'pending-stalled');
  assert.equal(f.handle.diagnostics.counts.submitted, 0);
  s.close(); finish(); await painting;
  assert.equal(f.handle.diagnostics.counts.submitted, 0);
  assert.equal(f.handle.diagnostics.counts.ackSent, 0);
  assert.equal(f.handle.diagnostics.retained, false); f.handle.dispose();
});

test('fault diagnostics retain only safe reasons and expose repair and reconnect', async () => {
  const f = await fixture({ present() { throw Error('secret payload and key'); } });
  const s = f.sockets[0]; s.state(); s.frame(); await f.raf(0);
  assert.equal(f.handle.diagnostics.progress, 'repair-pending');
  f.advance(6000); assert.equal(f.handle.diagnostics.progress, 'repair-stalled');
  assert.equal(f.handle.diagnostics.counts.repairs, 1);
  s.onerror({ message: 'secret payload and key' });
  const d = f.handle.diagnostics;
  assert.equal(d.progress, 'unknown'); assert.equal(d.counts.reconnects, 1);
  assert.ok(d.history.some(e => e.event === 'socket-error'));
  assert.ok(!f.handle.exportDiagnostics().includes('secret'));
  f.timer(); const next = f.sockets[1]; next.state();
  assert.equal(f.handle.diagnostics.progress, 'idle');
  // Malformed control text is never exported, nor are close reasons.
  next.onmessage({ data: 'secret payload and key' });
  next.onclose({ reason: 'untrusted peer text' });
  const exported = f.handle.exportDiagnostics();
  for (const text of ['secret', 'untrusted', epoch, 'fence', 'scene']) assert.ok(!exported.includes(text));
  assert.ok(JSON.parse(exported).history.some(e => e.event === 'invalid-stream'));
  f.handle.dispose();
});

test('ACK diagnostics do not confuse a closed send path with acknowledged progress', async () => {
  const f = await fixture(); const s = f.sockets[0]; s.state(); s.frame();
  // Model a transport that is no longer writable before its close callback.
  s.readyState = 3; await f.raf(0);
  assert.equal(f.handle.diagnostics.counts.submitted, 1);
  assert.equal(f.handle.diagnostics.counts.ackSent, 0);
  assert.equal(f.handle.diagnostics.ackSent, null);
  assert.equal(f.handle.diagnostics.progress, 'ack-pending');
  f.advance(6000); assert.equal(f.handle.diagnostics.progress, 'ack-stalled');
  assert.equal(f.handle.diagnostics.reason, 'ack-send-failed');
  f.handle.dispose();
});

test('throwing ACK send records no ACK, clears retained output and requests repair safely', async () => {
  const f = await fixture(); const s = f.sockets[0]; s.state();
  const send = s.send.bind(s);
  s.send = value => { if (JSON.parse(value).type === 'ack') throw Error('secret transport text'); send(value); };
  s.frame(); await f.raf(0);
  assert.equal(f.handle.diagnostics.counts.submitted, 1);
  assert.equal(f.handle.diagnostics.counts.ackSent, 0);
  assert.equal(f.handle.diagnostics.retained, false);
  assert.equal(f.handle.diagnostics.progress, 'repair-pending');
  assert.ok(f.handle.diagnostics.history.some(e => e.event === 'ack-send-failed'));
  assert.ok(!f.handle.exportDiagnostics().includes('secret'));
  assert.deepEqual(s.sent, [{ type: 'repair' }]); f.handle.dispose();
});

test('repair send failure stays sanitized and schedules reconnect instead of leaking a rejection', async () => {
  const f = await fixture({ present() { throw Error('secret decode'); } });
  const s = f.sockets[0]; s.state(); s.send = () => { throw Error('secret transport'); };
  s.frame(); await f.raf(0);
  assert.equal(f.handle.diagnostics.progress, 'unknown');
  assert.equal(f.handle.diagnostics.counts.reconnects, 1);
  assert.ok(f.handle.diagnostics.history.some(e => e.event === 'socket-error'));
  assert.ok(!f.handle.exportDiagnostics().includes('secret'));
  f.handle.dispose();
});

test('sustained changing source traverses receive/decode/submission/ACK with bounded history', async () => {
  const f = await fixture(); const s = f.sockets[0]; s.state();
  for (let i = 1; i <= 1000; i++) {
    s.frame(); await f.raf(i * 20);
    const d = f.handle.diagnostics;
    assert.equal(d.submitted.revision, String(i));
    assert.equal(d.ackSent.revision, String(i));
    assert.equal(s.sent.at(-1).revision, String(i));
    assert.ok(d.history.length <= 64);
  }
  assert.equal(f.handle.diagnostics.counts.decoded, 1000);
  assert.equal(f.handle.diagnostics.counts.submitted, 1000);
  assert.equal(f.handle.diagnostics.counts.ackSent, 1000);
  assert.ok(f.handle.diagnostics.history.some(e => e.cursor?.revision === '1000'));
  f.handle.dispose();
});

test('invalid FPS rejected', async () => {
  for (const maxFps of [0, 61, NaN, Infinity]) await assert.rejects(fixture({ maxFps }));
});
