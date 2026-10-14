// Product adapter: Rust owns presentation layout; generic CanvasRenderer owns raster output.
import initPresenter, { AudienceRenderer } from './bmux_presenter.js';
import initCanvas, { CanvasRenderer } from './bmux_web_renderer.js';

const CAPABILITY = 'bmux.presenter.audience.v1';
const MAX_BYTES = 16 * 1024 * 1024;
let nextCanvasId = 0;
const initialized = Promise.all([initPresenter(), initCanvas()]);

export async function createDocumentRenderer({canvas}) {
  await initialized;
  const engine = new AudienceRenderer();
  const staging = document.createElement('canvas');
  staging.id = `bmux-presenter-staging-${++nextCanvasId}`;
  staging.hidden = true;
  // Author canvas{display:block} overrides the UA [hidden] rule. This backing
  // surface must never become a flex item or feed its raster size into viewport
  // measurement; CanvasRenderer still needs its connected, discoverable ID.
  staging.style.setProperty('display', 'none', 'important');
  document.body.appendChild(staging);
  let inFlight = 0;
  let presentationQueue = Promise.resolve();
  let outputGeneration = null;
  let output = null;
  let serial = 0;
  let freed = false;
  let committed = null;
  let inputPending = false;
  let localAbort = null;
  const loaded = new Map();
  const decoder = new TextDecoder('utf-8', {fatal:true});
  const abortError = () => new DOMException('Preparation superseded', 'AbortError');
  const live = (token, signal) => {
    if (freed || token !== serial || signal?.aborted) throw abortError();
  };
  function releaseOutput() { output?.free(); output = null; }

  async function prepare(envelope, viewport, options = {}, scrollRows = 0) {
    if (inFlight >= 2) throw new Error('Audience preparation backpressure');
    ++inFlight;
    try { return await prepareInner(envelope, viewport, options, scrollRows); }
    finally { --inFlight; }
  }

  async function prepareInner(envelope, viewport, {requestResource, signal} = {}, scrollRows = 0) {
    if (freed) throw new Error('Presenter renderer freed');
    const token = ++serial;
    engine.discard();
    if (scrollRows !== 0) engine.scroll_by(scrollRows);
    if (envelope.capability !== CAPABILITY) throw new Error('Unsupported audience capability');
    const bytes = envelope.content instanceof Uint8Array ? envelope.content : new Uint8Array(envelope.content);
    if (bytes.length > MAX_BYTES) throw new Error('Audience content byte limit');
    const content = decoder.decode(bytes);
    const payload = JSON.parse(content);
    const manifest = new Map();
    if (!Array.isArray(envelope.resources) || envelope.resources.length > 128) throw new Error('Invalid audience manifest');
    for (const resource of envelope.resources) {
      if (!/^[a-f0-9]{64}$/.test(resource.id) || resource.mime_type !== 'image/png' ||
          !Number.isSafeInteger(resource.length) || resource.length < 0 || resource.length > 8 * 1024 * 1024 ||
          manifest.has(resource.id)) throw new Error('Invalid audience image descriptor');
      manifest.set(resource.id, resource);
    }
    try {
      // Only mapped resources are requested; callback remains manifest scoped by the generic host.
      const mapped = new Set(Object.values(payload.assets ?? {}));
      if (Array.from(mapped).reduce((sum, id) => sum + (manifest.get(id)?.length ?? 0), 0) > 32 * 1024 * 1024)
        throw new Error('Audience mapped resource cache limit');
      for (const id of mapped) {
        const descriptor = manifest.get(id);
        if (!descriptor) throw new Error('Audience asset missing from manifest');
        // Refresh all mapped resources into the Rust LRU each prepare; no stale admission assumptions.
        let resource = loaded.get(id);
        if (!resource) {
          if (typeof requestResource !== 'function') throw new Error('Resource callback unavailable');
          const result = await requestResource(id);
          live(token, signal);
          resource = result instanceof Uint8Array ? result : new Uint8Array(result);
          if (resource.byteLength !== descriptor.length) throw new Error('Audience resource length mismatch');
          const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', resource));
          live(token, signal);
          if (Array.from(digest, value => value.toString(16).padStart(2, '0')).join('') !== id)
            throw new Error('Audience resource SHA mismatch');
          // Bound the JS copy separately from the Rust resource cache.
          if (Array.from(loaded.values()).reduce((sum, value) => sum + value.byteLength, 0) + resource.byteLength > 32 * 1024 * 1024)
            loaded.clear();
          loaded.set(id, resource);
        }
        if (resource.byteLength !== descriptor.length) throw new Error('Reused audience resource descriptor mismatch');
        engine.add_resource(id, resource);
      }
      live(token, signal);
      const {width, height, device_pixel_ratio: ratio = 1} = viewport;
      if (![width, height, ratio].every(Number.isFinite) || width <= 0 || height <= 0 || ratio <= 0 || ratio > 8)
        throw new Error('Invalid audience viewport');
      // Cell metrics are backend policy, not slide layout. Rust performs all wrapping and responsive composition.
      const cellWidth = 9 * ratio;
      const cellHeight = 18 * ratio;
      const cols = Math.max(1, Math.floor(width / 9));
      const rows = Math.max(1, Math.floor(height / 18));
      if (cols > 65535 || rows > 65535 || cols * rows > 262144) throw new Error('Audience viewport cell limit');
      const frame = engine.prepare(content, cols, rows, envelope.generation, BigInt(envelope.revision));
      live(token, signal);
      const json = decoder.decode(frame);
      const presentation = presentationQueue.catch(() => {}).then(async () => {
        live(token, signal);
        if (!output) output = new CanvasRenderer(staging.id, cellWidth, cellHeight);
        else output.set_metrics(cellWidth, cellHeight, `${cellHeight * 0.8}px monospace`, cellHeight * 0.8);
        if (outputGeneration !== null && outputGeneration !== envelope.generation) await output.repair_json(json);
        else await output.present_json(json);
        outputGeneration = envelope.generation;
        live(token, signal);
        const expectedWidth = Math.ceil(cols * cellWidth);
        const expectedHeight = Math.ceil(rows * cellHeight);
        if (staging.width !== expectedWidth || staging.height !== expectedHeight)
          throw new Error('Audience Canvas geometry differs from prepared viewport');
        // The reusable renderer may repaint staging for later metrics/transactions.
        // Commit must read an immutable snapshot, never that mutable backing canvas.
        const preparedCanvas = document.createElement('canvas');
        preparedCanvas.width = expectedWidth;
        preparedCanvas.height = expectedHeight;
        const preparedContext = preparedCanvas.getContext('2d');
        if (!preparedContext) throw new Error('Audience prepared canvas context unavailable');
        preparedContext.drawImage(staging, 0, 0);
        return {metadata:output.metadata_json(), canvas:preparedCanvas};
      });
      presentationQueue = presentation;
      const preparedOutput = await presentation;
      const preparedMetadata = preparedOutput.metadata;
      const preparedCanvas = preparedOutput.canvas;
      const releasePrepared = () => { preparedCanvas.width = 0; preparedCanvas.height = 0; };
      try { live(token, signal); } catch (error) { releasePrepared(); throw error; }
      let settled = false;
      return {
        commit() {
          if (settled) throw new Error('Audience transaction already settled');
          live(token, signal);
          const context = canvas.getContext('2d');
          if (!context) throw new Error('Audience canvas context unavailable');
          // Retain prior pixels and dimensions until both output and Rust authority commit succeed.
          const backup = document.createElement('canvas');
          backup.width = canvas.width; backup.height = canvas.height;
          backup.getContext('2d').drawImage(canvas, 0, 0);
          try {
            canvas.width = preparedCanvas.width; canvas.height = preparedCanvas.height;
            context.drawImage(preparedCanvas, 0, 0);
            engine.commit();
          } catch (error) {
            canvas.width = backup.width; canvas.height = backup.height;
            context.drawImage(backup, 0, 0);
            engine.discard();
            settled = true;
            releasePrepared();
            throw error;
          }
          settled = true;
          releasePrepared();
          committed = {envelope, viewport, requestResource, metadata:preparedMetadata};
        },
        discard() {
          if (settled) return;
          settled = true;
          releasePrepared();
          if (token === serial) { engine.discard(); }
        },
      };
    } catch (error) {
      if (token === serial) { engine.discard(); }
      throw error;
    }
  }

  // View-local input never changes authoritative slide/reveal state.
  async function scroll(rows) {
    if (!committed || inputPending || freed) return;
    inputPending = true;
    localAbort = new AbortController();
    const previous = committed;
    try {
      const transaction = await prepare(previous.envelope, previous.viewport,
        {requestResource:previous.requestResource, signal:localAbort.signal}, rows);
      transaction.commit();
    } catch (error) {
      if (error.name !== 'AbortError') console.error('Audience local scroll rejected', error);
    } finally { inputPending = false; localAbort = null; }
  }
  const wheel = event => { if (committed && event.deltaY !== 0) { event.preventDefault(); void scroll(event.deltaY > 0 ? 3 : -3); } };
  const key = event => {
    const offsets = {ArrowDown:1, ArrowUp:-1, PageDown:Math.max(1, Math.floor((committed?.viewport.height ?? 18) / 18) - 1),
      PageUp:-Math.max(1, Math.floor((committed?.viewport.height ?? 18) / 18) - 1), Home:-2147483647, End:2147483647};
    if (committed && Object.hasOwn(offsets, event.key)) { event.preventDefault(); void scroll(offsets[event.key]); }
  };
  if (!canvas.hasAttribute('tabindex')) canvas.tabIndex = 0;
  canvas.addEventListener('wheel', wheel, {passive:false});
  canvas.addEventListener('keydown', key);
  return {
    capability:CAPABILITY, prepare,
    inspectCommittedFrame() { return committed ? JSON.parse(committed.metadata) : null; },
    reset() { ++serial; localAbort?.abort(); committed = null; engine.reset(); loaded.clear(); presentationQueue.finally(() => { if (!freed) { output?.reset(); outputGeneration = null; } }).catch(() => {}); },
    free() {
      if (freed) return;
      freed = true; ++serial; localAbort?.abort();
      canvas.removeEventListener('wheel', wheel); canvas.removeEventListener('keydown', key);
      engine.free(); presentationQueue.finally(() => { releaseOutput(); staging.remove(); }).catch(() => {}); loaded.clear(); committed = null;
    },
  };
}
