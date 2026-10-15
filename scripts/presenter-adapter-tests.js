// Focused adapter contract tests; no WASM, browser, or generated assets needed.
// Coordinator command: node scripts/presenter-adapter-tests.js
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(`${__dirname}/presenter-adapter.js`, 'utf8')
  .replace(/^import .*;\n/gm, '')
  .replace('export async function createDocumentRenderer', 'async function createDocumentRenderer');

async function fixture() {
  const elements = [];
  const engines = [];
  const renderers = [];
  const paints = [];
  let failDraw = false;
  let failCommit = false;
  let metadata = 0;
  let projections = [];
  const composites = [];
  const body = {appendChild(element) { element.parent = this; }};
  const mount = {appendChild(element) { element.parent = this; }};
  const document = {
    body,
    createElement(tag) {
      assert.equal(tag, 'canvas');
      const listeners = new Map();
      let width = 300, height = 150;
      const element = {
        id:'', parent:null, hidden:false, pixels:null, listeners,
        style:{setProperty(name, value, priority) { this[name] = value; this[`${name}Priority`] = priority; }},
        get width() { return width; }, set width(value) {
          assert.ok(value <= 16384 && value * height <= 16777216, 'bounded intermediate width allocation');
          width = value; this.pixels = null;
        },
        get height() { return height; }, set height(value) {
          assert.ok(value <= 16384 && width * value <= 16777216, 'bounded intermediate height allocation');
          height = value; this.pixels = null;
        },
        getContext() {
          return {save() {}, restore() {}, beginPath() {}, rect(...args) { composites.push({clip:args}); }, clip() {},
            drawImage(other, ...args) {
            if (args.length) composites.push({destination:args, source:[other.width, other.height]});
            assert.ok(other.width > 0 && other.height > 0, 'cannot draw a zero-size canvas');
            if (element.id === 'bmux' && failDraw) { failDraw = false; throw new Error('draw failed'); }
            element.pixels = other.pixels;
          }};
        },
        hasAttribute() { return false; },
        addEventListener(name, fn) { listeners.set(name, fn); },
        removeEventListener(name) { listeners.delete(name); },
        remove() { this.parent = null; },
      };
      elements.push(element);
      return element;
    },
  };
  class AudienceRenderer {
    constructor() { this.pending = null; this.committed = null; this.scroll = []; engines.push(this); }
    discard() { this.pending = null; }
    scroll_by(rows) { this.scroll.push(rows); }
    add_resource(id, bytes) { this.resource = {id, bytes}; }
    prepare(content, cols, rows, generation, revision) {
      this.pending = {cols, rows, generation, revision:String(revision)};
      paints.push(this.pending);
      return new TextEncoder().encode(JSON.stringify(this.pending));
    }
    pending_surfaces_json() { return JSON.stringify(projections); }
    commit() {
      if (failCommit) { failCommit = false; throw new Error('engine failed'); }
      this.committed = this.pending; this.pending = null;
    }
    reset() { this.pending = null; this.committed = null; }
    free() { this.freed = true; }
  }
  class CanvasRenderer {
    constructor(id, width, height) {
      assert.ok(width >= 1 && width <= 128);
      assert.ok(height >= 1 && height <= 256);
      this.canvas = elements.find(element => element.id === id);
      assert.ok(this.canvas.parent, 'staging must be discoverable');
      this.cellWidth = width; this.cellHeight = height;
      renderers.push(this);
    }
    set_metrics(width, height) {
      assert.ok(this.canvas.id.startsWith('bmux-presenter-surface-'), 'must not repaint old main grid');
      this.cellWidth = width; this.cellHeight = height;
    }
    async present_json(json) {
      const frame = JSON.parse(json);
      const width = Math.ceil((frame.cols ?? frame.buffer.area.width) * this.cellWidth);
      const height = Math.ceil((frame.rows ?? frame.buffer.area.height) * this.cellHeight);
      assert.ok(width <= 16384 && height <= 16384 && width * height <= 16777216);
      this.canvas.width = width; this.canvas.height = height;
      this.canvas.pixels = ++metadata;
      this.metadata = {...frame, metadata};
    }
    async repair_json(json) { await this.present_json(json); }
    metadata_json() { return JSON.stringify(this.metadata); }
    free() { this.freed = true; }
  }
  const context = vm.createContext({document, AudienceRenderer, CanvasRenderer,
    initPresenter:async () => {}, initCanvas:async () => {}, TextDecoder, TextEncoder,
    DOMException, AbortController, console});
  vm.runInContext(`${source}\nglobalThis.create = createDocumentRenderer;`, context);
  const adapter = await context.create({mount});
  const visible = elements.find(element => element.id === 'bmux');
  const staging = elements.find(element => element.hidden);
  assert.equal(visible.parent, mount);
  assert.equal(staging.parent, body);
  assert.equal(staging.style.display, 'none');
  assert.equal(staging.style.displayPriority, 'important');
  const envelope = revision => ({capability:'bmux.presenter.audience.v1', generation:'deck',
    revision:String(revision), content:new TextEncoder().encode('{}'), resources:[]});
  const viewport = (width, height, ratio = 1) => ({width, height, device_pixel_ratio:ratio});
  const snapshot = () => JSON.stringify({width:visible.width, height:visible.height,
    cssWidth:visible.style.width, cssHeight:visible.style.height, pixels:visible.pixels,
    authority:engines[0].committed, metadata:adapter.inspectCommittedFrame()});
  return {adapter, visible, staging, elements, engines, renderers, paints, envelope, viewport, snapshot, composites,
    project(value) { projections = value; },
    failDraw() { failDraw = true; }, failCommit() { failCommit = true; }};
}

async function main() {
  const f = await fixture();
  let transaction = await f.adapter.prepare(f.envelope(1), f.viewport(1001, 601));
  assert.equal(f.adapter.inspectCommittedFrame(), null);
  transaction.commit();
  assert.equal(f.visible.style.width, '999px');
  assert.equal(f.visible.style.height, '594px');
  assert.equal(f.visible.width, 999);
  assert.equal(f.visible.height, 594);
  const logical = {...f.paints.at(-1)};
  const firstWidth = f.visible.width;
  transaction = await f.adapter.prepare(f.envelope(1), f.viewport(1001, 601, 2));
  transaction.commit();
  assert.deepEqual(f.paints.at(-1), logical, 'DPR does not change composition');
  assert.equal(f.visible.width, firstWidth * 2);
  assert.equal(f.visible.style.width, '999px');
  assert.equal(f.visible.style.height, '594px');
  assert.equal(f.visible.height, 1188);
  assert.equal(f.renderers[0].freed, true);

  // Acceptance regression: no independent-axis stretch into viewport leftovers.
  transaction = await f.adapter.prepare(f.envelope(1), f.viewport(1280, 772));
  transaction.commit();
  assert.equal(f.visible.style.width, '1278px');
  assert.equal(f.visible.style.height, '756px');
  assert.equal(f.visible.width, 1278);
  assert.equal(f.visible.height, 756);

  // Same-density tall/wide transitions must not allocate newWidth * oldHeight.
  for (const viewport of [f.viewport(900, 16000), f.viewport(16000, 900)]) {
    transaction = await f.adapter.prepare(f.envelope(2), viewport);
    transaction.commit();
  }

  // Huge requested DPR and both dimension/pixel caps preserve the CSS grid.
  for (const viewport of [f.viewport(16000, 2000, 100), f.viewport(8000, 4000, 4),
    f.viewport(300, 30000, 4), f.viewport(30000, 300, 4), f.viewport(7, 11, 0.5)]) {
    transaction = await f.adapter.prepare(f.envelope(2), viewport);
    transaction.commit();
    const logicalWidth = Math.max(1, Math.floor(viewport.width / 9)) * 9;
    const logicalHeight = Math.max(1, Math.floor(viewport.height / 18)) * 18;
    assert.equal(f.visible.style.width, `${logicalWidth}px`);
    assert.equal(f.visible.style.height, `${logicalHeight}px`);
    const renderer = f.renderers.at(-1);
    const density = renderer.cellWidth / 9;
    assert.ok(Math.abs(renderer.cellHeight / 18 - density) < 1e-12, 'uniform raster density');
    const widthRounding = f.visible.width - logicalWidth * density;
    const heightRounding = f.visible.height - logicalHeight * density;
    assert.ok(widthRounding >= -1e-8 && widthRounding <= 1, 'backing width matches CSS times density');
    assert.ok(heightRounding >= -1e-8 && heightRounding <= 1, 'backing height matches CSS times density');
    assert.equal(f.paints.at(-1).cols, Math.max(1, Math.floor(viewport.width / 9)));
    assert.equal(f.paints.at(-1).rows, Math.max(1, Math.floor(viewport.height / 18)));
  }

  // A failed pixel write or Rust commit restores pixels, backing, CSS and metadata.
  for (const fail of [f.failDraw, f.failCommit]) {
    const previous = f.snapshot();
    transaction = await f.adapter.prepare(f.envelope(3), f.viewport(1600, 900, 2));
    fail();
    assert.throws(() => transaction.commit(), /failed/);
    assert.equal(f.snapshot(), previous);
    transaction.discard();
    assert.throws(() => transaction.commit(), /settled/);
  }

  // Supersession releases an uncommitted snapshot even if its caller retains it.
  const stale = await f.adapter.prepare(f.envelope(3), f.viewport(600, 400));
  const staleCanvas = f.elements.at(-1);
  transaction = await f.adapter.prepare(f.envelope(4), f.viewport(800, 500));
  assert.equal(staleCanvas.width, 0);
  assert.throws(() => stale.commit(), /settled/);
  transaction.commit();
  const previous = f.snapshot();
  const controller = new AbortController();
  transaction = await f.adapter.prepare(f.envelope(5), f.viewport(500, 500), {signal:controller.signal});
  const abortedCanvas = f.elements.at(-1);
  controller.abort();
  assert.throws(() => transaction.commit(), /superseded/);
  assert.equal(f.snapshot(), previous);
  assert.equal(abortedCanvas.width, 0);

  // Scroll stays internal, uses committed viewport, and respects browser shortcuts.
  const mutableViewport = f.viewport(800, 500, 2);
  transaction = await f.adapter.prepare(f.envelope(6), mutableViewport);
  transaction.commit();
  mutableViewport.width = 100;
  let prevented = 0;
  const key = f.visible.listeners.get('keydown');
  key({key:'ArrowDown', ctrlKey:true, preventDefault() { prevented++; }});
  assert.equal(prevented, 0);
  key({key:'ArrowDown', preventDefault() { prevented++; }});
  // Drain only Promise-based preparation; no browser runtime or timers.
  for (let i = 0; i < 20; ++i) await Promise.resolve();
  assert.equal(prevented, 1);
  assert.equal(f.engines[0].scroll.at(-1), 1);
  assert.equal(f.paints.at(-1).cols, Math.floor(800 / 9));
  assert.equal(f.paints.at(-1).revision, '6');
  const wheel = f.visible.listeners.get('wheel');
  wheel({deltaY:10, ctrlKey:true, preventDefault() { prevented++; }});
  assert.equal(prevented, 1);

  // An unsupported logical grid rejects instead of silently changing composition.
  await assert.rejects(f.adapter.prepare(f.envelope(7), f.viewport(200000, 18)), /logical grid/);
  for (let i = 0; i < 10; ++i) {
    transaction = await f.adapter.prepare(f.envelope(8), f.viewport(900, 600));
    transaction.discard();
  }
  // Only visible and staging retain backing storage after discarded transactions.
  assert.ok(f.elements.filter(element => element.width && element.height).length <= 2);
  transaction = await f.adapter.prepare(f.envelope(9), f.viewport(900, 600));
  const resetCanvas = f.elements.at(-1);
  f.adapter.reset();
  assert.equal(resetCanvas.width, 0);
  assert.equal(f.adapter.inspectCommittedFrame(), null);
  assert.throws(() => transaction.commit(), /settled/);
  f.adapter.free();
  for (let i = 0; i < 20; ++i) await Promise.resolve();
  assert.equal(f.visible.parent, null);
  assert.equal(f.staging.parent, null);
  assert.equal(f.visible.width, 0);
  assert.equal(f.staging.width, 0);
  assert.equal(f.visible.listeners.size, 0);
  // The nested authored terminal scales, while the main grid metrics do not.
  const p = await fixture();
  p.project([{block_id:'demo', x:1, y:4, width:40, height:10,
    clip:{x:1,y:4,width:40,height:10}, frame:{buffer:{area:{width:120,height:30}}}}]);
  const projected = await p.adapter.prepare(p.envelope(1), p.viewport(390, 844, 2));
  assert.equal(p.adapter.inspectCommittedFrame(), null);
  projected.commit();
  assert.deepEqual(p.composites.find(value => value.destination?.length === 4).destination, [18,144,720,360]);
  assert.equal(p.adapter.inspectCommittedFrame().embedded_surfaces[0].source_width, 120);
  assert.equal(p.adapter.inspectCommittedFrame().embedded_surfaces[0].width, 40);
  assert.ok(p.elements.filter(e => e.id.startsWith('bmux-presenter-surface-')).every(e => !e.parent && !e.width && !e.height));
  const retained = p.snapshot();
  p.failCommit();
  const rejected = await p.adapter.prepare(p.envelope(2), p.viewport(390,844,2));
  assert.throws(() => rejected.commit(), /engine failed/);
  assert.equal(p.snapshot(), retained);
  p.adapter.free();
  // No crypto global: exercise the actual HTTP-compatible host verifier through
  // the production image adapter, not just a resource-fetch-only test adapter.
  const bootstrap = fs.readFileSync('packages/web-renderer/browser/bootstrap.js', 'utf8');
  const digestContext = vm.createContext({stopped:false, setTimeout});
  vm.runInContext(bootstrap.slice(bootstrap.indexOf('async function resourceDigest'),
    bootstrap.indexOf('function documentAdapterPath')), digestContext);
  const resourceDigest = bytes => {
    digestContext.bytes = bytes;
    return vm.runInContext('resourceDigest(bytes)', digestContext);
  };
  const image = new Uint8Array([1, 2, 3]);
  const id = require('node:crypto').createHash('sha256').update(image).digest('hex');
  const images = await fixture();
  const envelope = images.envelope(1);
  envelope.content = new TextEncoder().encode(JSON.stringify({assets:{image:id}}));
  envelope.resources = [{id, mime_type:'image/png', length:3}];
  const options = {requestResource:async () => image, resourceDigest};
  const imageTransaction = await images.adapter.prepare(envelope, images.viewport(800, 600), options);
  imageTransaction.commit();
  const imageSnapshot = images.snapshot();
  assert.equal(images.engines[0].resource.id, id);
  const corruptEnvelope = {...envelope, revision:'2'};
  const otherId = require('node:crypto').createHash('sha256').update(new Uint8Array([4, 5, 6])).digest('hex');
  corruptEnvelope.content = new TextEncoder().encode(JSON.stringify({assets:{image:otherId}}));
  corruptEnvelope.resources = [{id:otherId, mime_type:'image/png', length:3}];
  await assert.rejects(images.adapter.prepare(corruptEnvelope, images.viewport(800, 600), {
    ...options, requestResource:async () => image,
  }), /Audience resource SHA mismatch/);
  assert.equal(images.snapshot(), imageSnapshot, 'failed integrity check replaced committed output');
  // A new renderer has no verified cache; wrong bytes must not change its frame.
  const corrupt = await fixture();
  const retainedCorrupt = corrupt.snapshot();
  await assert.rejects(corrupt.adapter.prepare(envelope, corrupt.viewport(800, 600), {
    ...options, requestResource:async () => new Uint8Array([3, 2, 1]),
  }), /Audience resource SHA mismatch/);
  assert.equal(corrupt.snapshot(), retainedCorrupt);
  assert.equal(images.snapshot(), imageSnapshot);
  corrupt.adapter.free();
  images.adapter.free();
  console.log('presenter adapter contracts and HTTP image verification: PASS');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
