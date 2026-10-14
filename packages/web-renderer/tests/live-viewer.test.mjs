import test from 'node:test';
import assert from 'node:assert/strict';
import { mountLiveViewer, revision, validateStatus } from '../browser/live-viewer.js';
import { mountCanvasViewer } from '../browser/canvas-viewer.js';
import { mountLivePage } from '../browser/live-page.js';

const epoch = '11111111-1111-4111-8111-111111111111';
const secondEpoch = '22222222-2222-4222-8222-222222222222';
const bytes = Uint8Array.of(66, 77, 87, 80, 3, 0);
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
function statusValue(state = 'live', overrides = {}) {
  return { protocol: 1, renderer: 'bmwp-3', state, epoch, revision: '1', lease_expires_at: Date.now() + 15000, ...overrides };
}
function frameResponse(overrides = {}) {
  return new Response(bytes, { headers: { 'Content-Type': 'application/octet-stream',
    'X-Bmux-Epoch': epoch, 'X-Snapshot-Revision': '1', ...overrides } });
}
async function fixture() {
  let value = statusValue();
  let frame = () => Promise.resolve(frameResponse());
  let presentation = async (_bytes, current) => current();
  let visible = false;
  let clears = 0;
  const requests = [];
  const status = { textContent: '' };
  const handle = await mountLiveViewer({
    container: { ownerDocument: { baseURI: 'https://bmux.test/live' } }, status,
    apiBase: '/api/live/main', assetBase: '/live-assets/', autoStart: false,
    canvasFactory: async () => ({
      clear() { visible = false; clears++; }, dispose() { visible = false; },
      async present(data, current) { const result = await presentation(data, current); if (result && current()) visible = true; return result; },
    }),
    fetchImpl: async (url, options) => {
      requests.push({ url, options });
      if (url.endsWith('/status')) return new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
      return frame();
    },
  });
  return { handle, status, requests, get visible() { return visible; }, get clears() { return clears; },
    setStatus(next) { value = next; }, setFrame(next) { frame = next; }, setPresentation(next) { presentation = next; } };
}

test('explicit protocol, epoch, lease and lossless u64 compatibility', () => {
  assert.equal(revision('18446744073709551615'), '18446744073709551615');
  for (const value of ['01', '-1', '18446744073709551616', 1, null]) assert.throws(() => revision(value));
  assert.throws(() => validateStatus(statusValue('live', { renderer: 'bmwp-2' })));
  assert.throws(() => validateStatus(statusValue('live', { epoch: '../private' })));
  assert.equal(validateStatus(statusValue('live', { lease_expires_at: 1 })).state, 'offline');
});

test('successful public rendering sends no authorization or authority requests and disposes', async () => {
  const f = await fixture();
  try {
    await f.handle.pollStatus(); await f.handle.pollFrame();
    assert.equal(f.visible, true);
    await f.handle.pollFrame();
    assert.match(f.requests.at(-1).url, /after=1$/);
    for (const { url, options } of f.requests) {
      assert.match(url, /^https:\/\/bmux.test\/api\/live\/main\/(status|presentation)/);
      assert.equal(options.credentials, 'omit');
      assert.equal(options.redirect, 'error');
      assert.equal(options.headers, undefined);
      assert.equal(options.method, undefined);
    }
  } finally { f.handle.dispose(); }
  assert.equal(f.visible, false);
});

for (const transition of ['paused', 'offline', 'epoch', 'revision', 'incompatible']) {
  test(`${transition} invalidates a pending async commit`, async () => {
    const f = await fixture();
    const entered = deferred(); const release = deferred();
    f.setPresentation(async (_data, current) => { entered.resolve(); await release.promise; return current(); });
    try {
      await f.handle.pollStatus();
      const pending = f.handle.pollFrame();
      await entered.promise;
      f.setStatus(transition === 'epoch' ? statusValue('live', { epoch: secondEpoch })
        : transition === 'revision' ? statusValue('live', { revision: '2' })
        : transition === 'incompatible' ? statusValue('live', { protocol: 2 }) : statusValue(transition));
      await f.handle.pollStatus();
      release.resolve(); await pending;
      assert.equal(f.visible, false);
      assert.ok(f.clears >= 2);
    } finally { release.resolve(); f.handle.dispose(); }
  });
}

test('unmount fences in-flight response even if fetch ignores abort', async () => {
  const f = await fixture(); const response = deferred();
  f.setFrame(() => response.promise);
  await f.handle.pollStatus(); const pending = f.handle.pollFrame();
  f.handle.dispose(); response.resolve(frameResponse()); await pending;
  assert.equal(f.visible, false);
  assert.ok(f.requests.at(-1).options.signal.aborted);
});

test('independent status polling works while frame request stalls; frame requests do not overlap', async () => {
  const f = await fixture(); const response = deferred();
  try {
    f.setFrame(() => response.promise);
    await f.handle.pollStatus(); const pending = f.handle.pollFrame();
    await f.handle.pollFrame();
    assert.equal(f.requests.filter(r => r.url.includes('/presentation')).length, 1);
    f.setStatus(statusValue('paused')); await f.handle.pollStatus();
    assert.ok(f.requests.find(r => r.url.includes('/presentation')).options.signal.aborted);
    response.resolve(frameResponse()); await pending;
    assert.equal(f.visible, false);
  } finally { response.resolve(frameResponse()); f.handle.dispose(); }
});

test('offline/network failure and mismatched response cursor clear already visible output', async () => {
  const f = await fixture();
  try {
    await f.handle.pollStatus(); await f.handle.pollFrame(); assert.equal(f.visible, true);
    f.setFrame(async () => { throw new Error('private diagnostic'); });
    await f.handle.pollFrame(); assert.equal(f.visible, false);
    assert.ok(!f.status.textContent.includes('private'));
    await f.handle.pollStatus();
    f.setFrame(async () => frameResponse({ 'X-Snapshot-Revision': '2' }));
    await f.handle.pollFrame(); assert.equal(f.visible, false);
  } finally { f.handle.dispose(); }
});

test('legacy renderer negotiation is rejected before fetching or painting a frame', async () => {
  const f = await fixture();
  try {
    f.setStatus(statusValue('live', { renderer: 'bmwp-2' }));
    await f.handle.pollStatus();
    await f.handle.pollFrame();
    assert.equal(f.visible, false);
  } finally { f.handle.dispose(); }
});

test('body bound and incompatible BMWP reject without presenting', async () => {
  const f = await fixture();
  try {
    await f.handle.pollStatus();
    f.setFrame(async () => frameResponse({ 'Content-Length': '8388609' }));
    await f.handle.pollFrame(); assert.equal(f.visible, false);
    await f.handle.pollStatus();
    f.setFrame(async () => new Response(Uint8Array.of(66,77,87,80,2), { headers: {
      'Content-Type': 'application/octet-stream', 'X-Bmux-Epoch': epoch, 'X-Snapshot-Revision': '1' } }));
    await f.handle.pollFrame(); assert.equal(f.visible, false);
  } finally { f.handle.dispose(); }
});

test('lease deadline clears live content without awaiting next status poll', async () => {
  const f = await fixture();
  try {
    f.setStatus(statusValue('live', { lease_expires_at: Date.now() + 40 }));
    await f.handle.pollStatus(); await f.handle.pollFrame(); assert.equal(f.visible, true);
    await new Promise(resolve => setTimeout(resolve, 60));
    assert.equal(f.visible, false);
  } finally { f.handle.dispose(); }
});

test('SPA route changes, delayed mount and pagehide dispose viewers without leaking listeners', async () => {
  let root = null; let notify; let observerConnected = false;
  const listeners = new Map(); let disposals = 0; let mounts = 0;
  const delayed = deferred();
  const doc = { documentElement: {}, getElementById(id) {
    return id === 'live-broadcast' ? root : { textContent: '' };
  } };
  const handle = mountLivePage({ doc,
    Observer: class { constructor(callback) { notify = callback; }
      observe() { observerConnected = true; } disconnect() { observerConnected = false; } },
    events: { addEventListener(name, callback) { listeners.set(name, callback); },
      removeEventListener(name) { listeners.delete(name); } },
    viewerFactory: async ({ apiBase }) => {
      assert.equal(apiBase, '/api/live/main/');
      mounts++;
      if (mounts === 1) await delayed.promise;
      return { dispose() { disposals++; } };
    },
  });
  assert.equal(mounts, 0);
  root = { isConnected: true, style: {} }; notify(); assert.equal(mounts, 1);
  notify(); assert.equal(mounts, 1);
  root.isConnected = false; root = null; notify();
  delayed.resolve(); await new Promise(resolve => setImmediate(resolve));
  assert.equal(disposals, 1);
  root = { isConnected: true, style: {} }; notify();
  await new Promise(resolve => setImmediate(resolve)); assert.equal(mounts, 2);
  listeners.get('pagehide')(); assert.equal(disposals, 2); assert.equal(observerConnected, false);
  listeners.get('pageshow')(); await new Promise(resolve => setImmediate(resolve));
  assert.equal(mounts, 3); assert.equal(observerConnected, true);
  handle.dispose(); assert.equal(disposals, 3); assert.equal(listeners.size, 0);
  assert.equal(observerConnected, false);
});

test('real Canvas adapter detaches pending WASM target and never reveals it after privacy clear', async () => {
  const children = new Set(); const entered = deferred(); const release = deferred();
  const doc = { baseURI: 'https://bmux.test/live', createElement() {
    return { hidden: false, style: {}, setAttribute() {}, remove() { children.delete(this); } };
  } };
  let frees = 0;
  const module = { default: async () => {}, CanvasRenderer: class {
    set_metrics() {} inspect_binary() { return '{"supported":true}'; }
    async present_binary() { entered.resolve(); await release.promise; }
    free() { frees++; }
  } };
  const viewer = await mountCanvasViewer({ container: { ownerDocument: doc, append(canvas) { children.add(canvas); } },
    assetBase: '/live-assets/', loadModule: async url => { assert.equal(url, 'https://bmux.test/live-assets/bmux_web_renderer.js'); return module; } });
  const pending = viewer.present(bytes, () => true); await entered.promise;
  assert.equal(children.size, 1);
  assert.equal([...children][0].hidden, true);
  viewer.clear(); assert.equal(children.size, 0);
  release.resolve(); assert.equal(await pending, false);
  assert.equal(children.size, 0); assert.equal(frees, 1);
  assert.equal(await viewer.present(bytes, () => true), true);
  assert.equal([...children][0].hidden, false);
  viewer.dispose(); assert.equal(children.size, 0);
});
