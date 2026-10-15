// Execute the actual generic bootstrap with a product-neutral mock adapter.
// Run: node packages/web-renderer/tests/document-bootstrap.js
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');
let source = fs.readFileSync(`${__dirname}/../browser/bootstrap.js`, 'utf8');
source = source.split('\nconst delivery =')[0]
  .replace('await import(documentAdapterPath(adapterPath))', 'await loadAdapter(documentAdapterPath(adapterPath))');

async function audience(width) {
  const paths = [];
  const commits = [];
  let discarded = 0;
  let prepared = 0;
  let reset = 0;
  let freed = 0;
  let context;
  const listeners = new Map();
  const observers = [];
  const image = Uint8Array.from([1, 2, 3]);
  const id = Buffer.from(await webcrypto.subtle.digest('SHA-256', image)).toString('hex');
  const envelope = revision => ({ version: 1, publication_revision: String(revision),
    generation: 'source', revision: String(revision), capability: 'test.content.v1',
    content: [123, 125], resources: [{ id, mime_type: 'image/png', length: 3 }] });
  let polls = 0;
  const status = { textContent: '' };
  const mount = { getBoundingClientRect: () => ({ width, height: 480 }) };
  const dprListeners = new Set();
  const visualListeners = new Map();
  let mounted = false;
  const adapter = {
    capability: 'test.content.v1',
    async prepare(document, viewport, { requestResource, signal }) {
      assert.equal(signal.aborted, false);
      assert.equal(viewport.width, width);
      assert.equal(viewport.height, 480);
      assert.equal(viewport.device_pixel_ratio, 0.75);
      assert.equal(mounted, true);
      assert.equal(Object.isFrozen(document), true);
      assert.deepEqual(Array.from(await requestResource(id)), [1, 2, 3]);
      prepared++;
      // Resize during async preparation must discard, never display obsolete layout.
      if (prepared === 1) observers[0]();
      let used = false;
      return {
        commit() {
          assert.equal(used, false);
          // Failed commit must not advance delivery cursor; retry is full-only.
          if (prepared === 3) throw new Error('injected commit failure');
          used = true;
          commits.push(document.revision);
        },
        discard() { assert.equal(used, false); used = true; discarded++; },
      };
    },
    inspectCommittedFrame() { return { public: true, revision: commits.at(-1),
      partial_metadata: ['semantics', 'selection'], unsupported_metadata: ['selection'],
      selection_text_available: false }; },
    reset() { reset++; }, free() { freed++; },
  };
  const globals = {
    URLSearchParams, TextDecoder, Uint8Array, BigInt, AbortController, Response,
    crypto: webcrypto, performance, console, devicePixelRatio: 2,
      location: { hash: '#token=secret', pathname: '/', search: '?inspect=1' },
    window: { devicePixelRatio: 0.75, visualViewport: {
      addEventListener: (name, fn) => visualListeners.set(name, fn),
      removeEventListener: name => visualListeners.delete(name),
    } },
    history: { replaceState() {} },
    document: {
      getElementById: name => {
        if (name === 'status') return status;
        if (name === 'viewport') return mount;
        throw new Error('generic document host must not look up a Canvas');
      },
      createElement() { throw new Error('generic document host must not initialize Canvas'); },
      fonts: { ready: Promise.resolve(), addEventListener() {}, removeEventListener() {} },
    },
    addEventListener: (name, fn) => listeners.set(name, fn),
    removeEventListener: name => listeners.delete(name),
    ResizeObserver: class { constructor(fn) { observers.push(fn); } observe() {} disconnect() {} },
    setTimeout: (fn, ms) => setTimeout(fn, ms === 1000 ? 0 : ms), clearTimeout,
    requestAnimationFrame: fn => setTimeout(fn, 0), cancelAnimationFrame: clearTimeout,
    matchMedia: () => ({
      addEventListener: (name, fn) => dprListeners.add(fn),
      removeEventListener: (name, fn) => dprListeners.delete(fn),
    }),
    loadAdapter: async path => {
      assert.equal(path, '/adapter.js');
      return { createDocumentRenderer: async options => {
        assert.deepEqual(Object.keys(options), ['mount']);
        assert.equal(options.mount, mount);
        mounted = true;
        return adapter;
      } };
    },
    fetch: async (path, options) => {
      paths.push(path);
      assert.equal(options.headers.Authorization, 'Bearer secret');
      assert.equal(options.credentials, 'omit');
      if (path === `/resource/${id}`) return new Response(image, { headers: { 'Content-Type': 'image/png' } });
      assert.ok(path.startsWith('/document?version=1'));
      polls++;
      if (polls > 1) {
        const inspection = context.window.__bmuxDocumentInspection();
        assert.equal(inspection.revision, polls < 4 ? '1' : '2');
        assert.equal(inspection.viewport.width, width);
        assert.equal(inspection.frame.public, true);
        if (polls !== 3) {
          assert.ok(status.textContent.includes('Partial metadata: semantics, selection'));
          assert.ok(status.textContent.includes('Unavailable metadata: selection'));
          assert.ok(status.textContent.includes('Selection source text unavailable'));
        }
        assert.equal('content' in inspection, false);
      }
      if (polls === 4) {
        vm.runInContext('expire()', context);
        return new Response(null, { status: 204 });
      }
      const revision = polls === 1 ? 1 : 2;
      return new Response(JSON.stringify(envelope(revision)), {
        headers: { 'Content-Type': 'application/vnd.bmux.document-v1+json', 'X-Snapshot-Revision': String(revision) },
      });
    },
  };
  context = vm.createContext(globals);
  vm.runInContext(source, context);
  for (const path of ['https://other/adapter.js', '//other/adapter.js', '/adapter.js?x=1', '/x/../adapter.js', null]) {
    context.path = path;
    assert.throws(() => vm.runInContext('documentAdapterPath(path)', context));
  }
  context.valid = envelope(1);
  assert.equal(vm.runInContext('validateDocument(valid, "1").revision', context), '1');
  for (const mutate of [
    value => { value.version = 2; },
    value => { value.revision = 9007199254740992; },
    value => { value.generation = 'bad\nidentity'; },
    value => { value.resources.push({ ...value.resources[0] }); },
    value => { value.resources[0].length = 8 * 1024 * 1024 + 1; },
    value => { value.content = [256]; },
    value => { value.publication_revision = '2'; },
  ]) {
    context.invalid = envelope(1);
    mutate(context.invalid);
    assert.throws(() => vm.runInContext('validateDocument(invalid, "1")', context));
  }
  await vm.runInContext('runDocument("/adapter.js")', context);
  assert.deepEqual(commits, ['1', '2']);
  assert.equal(discarded, 2);
  assert.equal(reset, 1);
  assert.equal(freed, 1);
  assert.equal(dprListeners.size, 0);
  assert.equal(visualListeners.size, 0);
  assert.equal(context.window.__bmuxDocumentInspection, undefined);
  assert.deepEqual(paths.filter(path => path.startsWith('/document')), [
    '/document?version=1', '/document?version=1&after=1', '/document?version=1', '/document?version=1&after=2',
  ]);
  assert.equal(paths.filter(path => path.startsWith('/resource')).length, 1);
  assert.ok(!paths.some(path => path === '/view' || path === '/viewport'));
  return commits;
}

// A layout invalidation arriving during a failed prepare must not be lost.
// This mock is deliberately DOM-only: there is no Canvas/WASM implementation.
async function failedReflow() {
  let context;
  let width = 300;
  let observer;
  let prepareCount = 0;
  let frame = null;
  let resolveReflow;
  const finished = new Promise(resolve => { resolveReflow = resolve; });
  const events = new Map();
  const fontEvents = new Map();
  const visualEvents = new Map();
  let densityChange;
  const mount = { getBoundingClientRect: () => ({ width, height: 200 }) };
  const adapter = {
    capability: 'test.content.v1',
    async prepare(document, viewport) {
      prepareCount++;
      if (prepareCount === 2) {
        assert.equal(viewport.width, 400);
        width = 500.25;
        context.window.devicePixelRatio = 3;
        // All notification sources coalesce, but invalidate in-flight work now.
        observer();
        events.get('resize')();
        fontEvents.get('loadingdone')();
        visualEvents.get('resize')();
        densityChange();
        await Promise.resolve();
        throw new Error('failed obsolete preparation');
      }
      return {
        commit() {
          frame = { ...viewport };
          if (prepareCount === 3) resolveReflow();
        },
        discard() {},
      };
    },
    inspectCommittedFrame: () => frame,
    reset() {}, free() {},
  };
  let polls = 0;
  context = vm.createContext({
    URLSearchParams, TextDecoder, Uint8Array, BigInt, AbortController, Response,
    performance, console,
    location: { hash: '', pathname: '/', search: '?inspect=1' },
    history: { replaceState() {} },
    window: { devicePixelRatio: 1, visualViewport: {
      addEventListener: (name, fn) => visualEvents.set(name, fn),
      removeEventListener: name => visualEvents.delete(name),
    } },
    document: {
      getElementById: name => name === 'status' ? { textContent: '' } : mount,
      fonts: { ready: new Promise(() => {}),
        addEventListener: (name, fn) => fontEvents.set(name, fn),
        removeEventListener: name => fontEvents.delete(name) },
    },
    addEventListener: (name, fn) => events.set(name, fn),
    removeEventListener: name => events.delete(name),
    ResizeObserver: class { constructor(fn) { observer = fn; } observe() {} disconnect() {} },
    matchMedia: () => ({ addEventListener: (name, fn) => { densityChange = fn; }, removeEventListener() {} }),
    requestAnimationFrame: fn => setTimeout(fn, 0), cancelAnimationFrame: clearTimeout,
    setTimeout, clearTimeout,
    loadAdapter: async () => ({ createDocumentRenderer: async ({ mount: target }) => {
      assert.equal(target, mount);
      return adapter;
    } }),
    fetch: async () => {
      if (++polls === 1) return new Response(JSON.stringify({ version: 1,
        publication_revision: '1', generation: 'source', revision: '1',
        capability: adapter.capability, content: [123, 125], resources: [] }), {
        headers: { 'Content-Type': 'application/vnd.bmux.document-v1+json', 'X-Snapshot-Revision': '1' },
      });
      width = 400;
      observer();
      await finished;
      const inspection = context.window.__bmuxDocumentInspection();
      assert.equal(inspection.revision, '1');
      assert.equal(inspection.viewport.width, 500.25);
      assert.equal(inspection.viewport.device_pixel_ratio, 3);
      vm.runInContext('expire()', context);
      return new Response(null, { status: 204 });
    },
  });
  vm.runInContext(source, context);
  const watchdog = setTimeout(() => { vm.runInContext('expire()', context); resolveReflow(); }, 2000);
  try { await vm.runInContext('runDocument("/adapter.js")', context); }
  finally { clearTimeout(watchdog); }
  assert.equal(prepareCount, 3);
  assert.equal(frame.width, 500.25);
  assert.equal(frame.device_pixel_ratio, 3);
}

(async () => {
  await failedReflow();
  assert.deepEqual(await audience(320.5), await audience(20000.25));
  console.log('PASS document validation, independent viewports, resource integrity/cache, superseded layout, atomic commit repair, lifecycle, no lease');
})().catch(error => { console.error(error); process.exitCode = 1; });
