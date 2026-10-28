import test from 'node:test';
import assert from 'node:assert/strict';
import { mountCanvasViewer } from '../browser/canvas-viewer.js';
import { mountLiveViewer } from '../browser/live-viewer.js';

// Real JS socket/scheduler/Canvas-adapter integration with a deterministic WASM
// stand-in. This does not claim compiled WASM, a real relay or physical display.
async function fixture() {
  const elements = new Map(), children = [], frames = new Map(), timers = new Map(), sockets = [];
  let clock = 0, id = 0, failDecode = false, failSubmit = false, decoded = 0, copies = 0;
  const doc = { baseURI: 'https://example.test/', createElement() {
    return { style: {}, width: 10, height: 20, setAttribute() {}, remove() {
      const index = children.indexOf(this); if (index >= 0) children.splice(index, 1);
    }, getContext() { return { drawImage(source) {
      if (failSubmit) throw Error('private Canvas failure');
      this.copied = source.value; copies++;
    } }; } };
  } };
  const container = { ownerDocument: doc, append(canvas) { children.push(canvas); elements.set(canvas.id, canvas); } };
  class Renderer {
    constructor(id) { this.canvas = elements.get(id); }
    set_metrics() {}
    inspect_binary(bytes) { return JSON.stringify({ supported: true, generation: 'private-generation', revision: String(new DataView(bytes.buffer).getUint32(5)) }); }
    async present_binary(bytes) {
      if (failDecode) throw Error('private decoder failure');
      this.canvas.value = new DataView(bytes.buffer).getUint32(5); decoded++;
    }
    free() {}
  }
  class Socket {
    constructor() { this.sent = []; this.readyState = 1; sockets.push(this); }
    send(value) { this.sent.push(JSON.parse(value)); }
    close() { this.readyState = 3; this.onclose?.({ reason: 'private peer text' }); }
    state(fence = '1') { this.onmessage({ data: JSON.stringify({ type: 'state', state: 'live', epoch: '11111111-1111-4111-8111-111111111111', fence, lease_ms: 15000, lease_remaining_ms: 15000 }) }); }
    frame(revision) {
      const bytes = new Uint8Array(9); bytes.set([66, 77, 87, 80, 5]);
      new DataView(bytes.buffer).setUint32(5, revision); this.onmessage({ data: bytes.buffer });
    }
  }
  const handle = await mountLiveViewer({ container, status: {}, assetBase: '/', WebSocketImpl: Socket,
    viewerFactory: options => mountCanvasViewer({ ...options, loadModule: async () => ({ default: async () => {}, CanvasRenderer: Renderer }) }),
    now: () => clock, requestFrame(fn) { frames.set(++id, fn); return id; }, cancelFrame(key) { frames.delete(key); },
    setTimer(fn) { timers.set(++id, fn); return id; }, clearTimer(key) { timers.delete(key); } });
  return { handle, sockets, children, frames, timers,
    failDecode(value) { failDecode = value; }, failSubmit(value) { failSubmit = value; },
    get decoded() { return decoded; }, get copies() { return copies; },
    async flush() { clock += 20; const work = [...frames.values()]; frames.clear(); await Promise.all(work.map(fn => fn(clock))); } };
}

test('changing binary source reaches actual JS Canvas adapter and matching local ACK for 1000 updates', async () => {
  const f = await fixture(), s = f.sockets[0]; s.state();
  for (let revision = 1; revision <= 1000; revision++) {
    s.frame(revision); await f.flush();
    assert.equal(s.sent.at(-1).revision, String(revision));
    assert.equal(f.handle.diagnostics.ackSent.revision, String(revision));
    assert.equal(f.children.filter(c => !c.hidden).length, 1);
    assert.equal(f.children.length, 2); // One retained decoder + one visible copy.
  }
  assert.equal(f.decoded, 1000); assert.equal(f.copies, 1000);
  assert.equal(f.handle.diagnostics.counts.decoded, 1000);
  assert.equal(f.handle.diagnostics.counts.submitted, 1000);
  assert.ok(f.handle.diagnostics.history.length <= 64);
  assert.ok(!f.handle.exportDiagnostics().includes('private-generation'));
  f.handle.dispose(); assert.equal(f.children.length, 0);
  assert.equal(f.frames.size, 0); assert.equal(f.timers.size, 0);
});

test('decoder failure versus Canvas submission failure have distinct progress counters and no false ACK', async () => {
  for (const phase of ['decode', 'submit']) {
    const f = await fixture(), s = f.sockets[0]; s.state();
    if (phase === 'decode') f.failDecode(true); else f.failSubmit(true);
    s.frame(1); await f.flush();
    const d = f.handle.diagnostics;
    assert.equal(d.counts.decoded, phase === 'decode' ? 0 : 1);
    assert.equal(d.counts.submitted, 0); assert.equal(d.counts.ackSent, 0);
    assert.equal(d.retained, false); assert.equal(d.progress, 'repair-pending');
    assert.deepEqual(s.sent, [{ type: 'repair' }]); assert.equal(f.children.length, 0);
    f.failDecode(false); f.failSubmit(false); s.frame(2); await f.flush();
    assert.equal(f.handle.diagnostics.submitted.revision, '2');
    assert.equal(f.handle.diagnostics.counts.ackSent, 1);
    assert.ok(!f.handle.exportDiagnostics().includes('private'));
    f.handle.dispose();
  }
});
