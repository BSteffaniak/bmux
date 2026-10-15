// Execute the actual generic bootstrap with a product-neutral mock adapter.
// Run: node packages/web-renderer/tests/document-bootstrap.js
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const { webcrypto } = require('node:crypto');
let source = fs.readFileSync(`${__dirname}/../browser/bootstrap.js`, 'utf8');
source = source.replace(/^import .*\n/, '').split('\nconst delivery =')[0]
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
  const adapter = {
    capability: 'test.content.v1',
    async prepare(document, viewport, { requestResource, signal }) {
      assert.equal(signal.aborted, false);
      assert.equal(viewport.width, width);
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
    window: {},
    history: { replaceState() {} },
    document: {
      getElementById: name => name === 'status' ? status : name === 'viewport'
        ? { getBoundingClientRect: () => ({ width, height: 480 }) } : {},
      fonts: { addEventListener() {}, removeEventListener() {} },
    },
    addEventListener: (name, fn) => listeners.set(name, fn),
    removeEventListener: name => listeners.delete(name),
    ResizeObserver: class { constructor(fn) { observers.push(fn); } observe() {} disconnect() {} },
    setTimeout: (fn, ms) => setTimeout(fn, ms === 1000 ? 0 : ms), clearTimeout,
    loadAdapter: async path => { assert.equal(path, '/adapter.js'); return { createDocumentRenderer: async () => adapter }; },
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
  assert.equal(context.window.__bmuxDocumentInspection, undefined);
  assert.deepEqual(paths.filter(path => path.startsWith('/document')), [
    '/document?version=1', '/document?version=1&after=1', '/document?version=1', '/document?version=1&after=2',
  ]);
  assert.equal(paths.filter(path => path.startsWith('/resource')).length, 1);
  assert.ok(!paths.some(path => path === '/view' || path === '/viewport'));
  return commits;
}

(async () => {
  assert.deepEqual(await audience(320), await audience(1200));
  console.log('PASS document validation, independent viewports, resource integrity/cache, superseded layout, atomic commit repair, lifecycle, no lease');
})().catch(error => { console.error(error); process.exitCode = 1; });
