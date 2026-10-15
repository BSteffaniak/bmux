// Product adapter: Rust owns presentation layout; generic CanvasRenderer owns raster output.
import initPresenter, { AudienceRenderer } from './bmux_presenter.js';
import initCanvas, { CanvasRenderer } from './bmux_web_renderer.js';

const CAPABILITY = 'bmux.presenter.audience.v1';
const MAX_BYTES = 16 * 1024 * 1024;
const MAX_CANVAS_DIMENSION = 16384;
const MAX_CANVAS_PIXELS = 16777216;

// Canvas metrics are raster policy only. Never shrink Rust's logical grid to
// satisfy a backing-store budget. Account for ceil rounding before allocating.
function rasterMetrics(cols, rows, requestedRatio) {
  const fits = ratio => {
    const width = Math.ceil(cols * (9 * ratio));
    const height = Math.ceil(rows * (18 * ratio));
    return width <= MAX_CANVAS_DIMENSION && height <= MAX_CANVAS_DIMENSION &&
      width * height <= MAX_CANVAS_PIXELS;
  };
  // The generic Canvas backend supports cell widths >= 1 and heights >= 1.
  let low = 1 / 9;
  if (!fits(low)) throw new Error('Audience logical grid exceeds Canvas raster limits');
  let high = Math.max(low, Math.min(requestedRatio, 128 / 9));
  if (fits(high)) low = high;
  else for (let i = 0; i < 48; ++i) {
    const middle = (low + high) / 2;
    if (fits(middle)) low = middle;
    else high = middle;
  }
  return {cellWidth:9 * low, cellHeight:18 * low};
}
let nextCanvasId = 0;
const initialized = Promise.all([initPresenter(), initCanvas()]);

export async function createDocumentRenderer({mount}) {
  await initialized;
  if (!mount || typeof mount.appendChild !== 'function') throw new Error('Audience mount unavailable');
  const canvas = document.createElement('canvas');
  canvas.id = 'bmux';
  canvas.width = 0; canvas.height = 0;
  canvas.style.width = '0px'; canvas.style.height = '0px';
  canvas.style.display = 'block';
  canvas.style.flex = 'none';
  mount.appendChild(canvas);
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
  let outputMetrics = null;
  let pendingTransaction = null;
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
  function releaseOutput() {
    output?.free(); output = null; outputGeneration = null; outputMetrics = null;
    staging.width = 0; staging.height = 0;
  }

  async function prepare(envelope, viewport, options = {}, scrollRows = 0) {
    if (inFlight >= 2) throw new Error('Audience preparation backpressure');
    ++inFlight;
    try {
      // Retained local scrolling must reuse the exact committed CSS viewport,
      // not a caller-owned object that can change during asynchronous work.
      const snapshot = Object.freeze({width:viewport.width, height:viewport.height,
        device_pixel_ratio:viewport.device_pixel_ratio});
      return await prepareInner(envelope, snapshot, options, scrollRows);
    }
    finally { --inFlight; }
  }

  async function prepareInner(envelope, viewport, {requestResource, resourceDigest, signal} = {}, scrollRows = 0) {
    if (freed) throw new Error('Presenter renderer freed');
    pendingTransaction?.discard();
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
          // The neutral host verifier supports HTTPS and HTTP LAN origins alike.
          if (typeof resourceDigest !== 'function') throw new Error('Resource digest callback unavailable');
          const digest = await resourceDigest(resource);
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
      if (![width, height, ratio].every(Number.isFinite) || width <= 0 || height <= 0 || ratio <= 0)
        throw new Error('Invalid audience viewport');
      // Cell metrics are backend policy, not slide layout. Rust performs all wrapping and responsive composition.
      const cols = Math.max(1, Math.floor(width / 9));
      const rows = Math.max(1, Math.floor(height / 18));
      if (cols > 65535 || rows > 65535 || cols * rows > 262144) throw new Error('Audience viewport cell limit');
      const {cellWidth, cellHeight} = rasterMetrics(cols, rows, ratio);
      const frame = engine.prepare(content, cols, rows, envelope.generation, BigInt(envelope.revision));
      // Capture the exact speculative Rust layout before another preparation.
      const projectionsText = engine.pending_surfaces_json();
      if (projectionsText.length > MAX_BYTES) throw new Error('Audience projection byte limit');
      const projections = JSON.parse(projectionsText);
      if (!Array.isArray(projections) || projections.length > 128) throw new Error('Audience projection limit');
      live(token, signal);
      const json = decoder.decode(frame);
      const presentation = presentationQueue.catch(() => {}).then(async () => {
        live(token, signal);
        // set_metrics repaints the OLD grid. Recreate on a density change so an
        // old wide/tall frame cannot allocate beyond the new frame's budget.
        const metrics = `${cellWidth},${cellHeight}`;
        if (outputMetrics !== metrics) releaseOutput();
        if (!output) {
          output = new CanvasRenderer(staging.id, cellWidth, cellHeight);
          outputMetrics = metrics;
        }
        // Clear the non-authoritative destination before a shape change. Setting
        // width first on an old tall canvas could transiently exceed the pixel
        // cap even when both old and new frame sizes individually fit.
        staging.width = 0; staging.height = 0;
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
        try {
          const preparedContext = preparedCanvas.getContext('2d');
          if (!preparedContext) throw new Error('Audience prepared canvas context unavailable');
          preparedContext.drawImage(staging, 0, 0);
          // Rust owns slot placement and clipping. Rasterize the complete authored
          // scene independently, then composite it before this transaction commits.
          for (const projection of projections) {
            live(token, signal);
            const {x, y, width:pw, height:ph, clip, frame:source} = projection;
            const area = source?.buffer?.area;
            if (![x, y, pw, ph, clip?.x, clip?.y, clip?.width, clip?.height].every(Number.isFinite) ||
                pw <= 0 || ph <= 0 || !area || !Number.isSafeInteger(area.width) ||
                !Number.isSafeInteger(area.height) || area.width <= 0 || area.height <= 0 ||
                area.width * area.height > 262144 || clip.x < 0 || clip.y < 0 ||
                clip.width < 0 || clip.height < 0 || clip.x + clip.width > cols || clip.y + clip.height > rows)
              throw new Error('Invalid audience surface projection');
            const sourceMetrics = rasterMetrics(area.width, area.height,
              Math.max(cellWidth / 9 * pw / area.width, cellHeight / 18 * ph / area.height));
            const nested = document.createElement('canvas');
            nested.id = `bmux-presenter-surface-${++nextCanvasId}`;
            nested.hidden = true;
            nested.style.setProperty('display', 'none', 'important');
            document.body.appendChild(nested);
            let renderer;
            try {
              renderer = new CanvasRenderer(nested.id, sourceMetrics.cellWidth, sourceMetrics.cellHeight);
              renderer.set_metrics(sourceMetrics.cellWidth, sourceMetrics.cellHeight,
                `${sourceMetrics.cellHeight * 0.8}px monospace`, sourceMetrics.cellHeight * 0.8);
              await renderer.present_json(projection.frame_json ?? JSON.stringify(source));
              live(token, signal);
              preparedContext.save();
              try {
                preparedContext.beginPath();
                preparedContext.rect(clip.x * cellWidth, clip.y * cellHeight,
                  clip.width * cellWidth, clip.height * cellHeight);
                preparedContext.clip();
                preparedContext.drawImage(nested, x * cellWidth, y * cellHeight, pw * cellWidth, ph * cellHeight);
              } finally { preparedContext.restore(); }
            } finally {
              renderer?.free(); nested.remove(); nested.width = 0; nested.height = 0;
            }
          }
          const metadata = JSON.parse(output.metadata_json());
          metadata.embedded_surfaces = projections.map(({frame:source, frame_json:_frameJson, ...placement}) =>
            ({...placement, source_width:source.buffer.area.width, source_height:source.buffer.area.height}));
          return {metadata:JSON.stringify(metadata), canvas:preparedCanvas};
        } catch (error) {
          preparedCanvas.width = 0; preparedCanvas.height = 0;
          throw error;
        }
      });
      presentationQueue = presentation;
      const preparedOutput = await presentation;
      const preparedMetadata = preparedOutput.metadata;
      const preparedCanvas = preparedOutput.canvas;
      const releasePrepared = () => { preparedCanvas.width = 0; preparedCanvas.height = 0; };
      try { live(token, signal); } catch (error) { releasePrepared(); throw error; }
      let settled = false;
      const transaction = {
        commit() {
          if (settled) throw new Error('Audience transaction already settled');
          let backup = null;
          let changed = false;
          const oldWidth = canvas.width, oldHeight = canvas.height;
          const oldCssWidth = canvas.style.width, oldCssHeight = canvas.style.height;
          try {
            live(token, signal);
            const context = canvas.getContext('2d');
            if (!context) throw new Error('Audience canvas context unavailable');
            // Rollback and one bounded nested surface store may also coexist.
            // Every store has the same hard pixel cap; nested stores are released serially.
            backup = document.createElement('canvas');
            backup.width = oldWidth; backup.height = oldHeight;
            const backupContext = backup.getContext('2d');
            if (!backupContext) throw new Error('Audience rollback canvas context unavailable');
            if (oldWidth && oldHeight) backupContext.drawImage(canvas, 0, 0);
            changed = true;
            canvas.width = 0; canvas.height = 0;
            canvas.width = preparedCanvas.width; canvas.height = preparedCanvas.height;
            context.drawImage(preparedCanvas, 0, 0);
            // Display the logical grid footprint without stretching cells to
            // absorb viewport remainders. The host centers the unused space.
            canvas.style.width = `${cols * 9}px`; canvas.style.height = `${rows * 18}px`;
            engine.commit();
            committed = {envelope, viewport, requestResource, resourceDigest, metadata:preparedMetadata};
          } catch (error) {
            try {
              if (changed) {
                canvas.width = 0; canvas.height = 0;
                canvas.width = oldWidth; canvas.height = oldHeight;
                canvas.style.width = oldCssWidth; canvas.style.height = oldCssHeight;
                if (oldWidth && oldHeight) canvas.getContext('2d').drawImage(backup, 0, 0);
              }
            } finally {
              if (token === serial) engine.discard();
            }
            throw error;
          } finally {
            settled = true;
            releasePrepared();
            if (backup) { backup.width = 0; backup.height = 0; }
            if (pendingTransaction === transaction) pendingTransaction = null;
          }
        },
        discard() {
          if (settled) return;
          settled = true;
          releasePrepared();
          if (pendingTransaction === transaction) pendingTransaction = null;
          if (token === serial) { engine.discard(); }
        },
      };
      pendingTransaction = transaction;
      return transaction;
    } catch (error) {
      if (token === serial) { engine.discard(); }
      throw error;
    }
  }

  // View-local input never changes authoritative slide/reveal state.
  async function scroll(rows) {
    if (!committed || inputPending || freed || inFlight || pendingTransaction) return;
    inputPending = true;
    localAbort = new AbortController();
    const previous = committed;
    try {
      const transaction = await prepare(previous.envelope, previous.viewport,
        {requestResource:previous.requestResource, resourceDigest:previous.resourceDigest, signal:localAbort.signal}, rows);
      transaction.commit();
    } catch (error) {
      if (error.name !== 'AbortError') console.error('Audience local scroll rejected', error);
    } finally { inputPending = false; localAbort = null; }
  }
  const wheel = event => {
    // Leave browser zoom and horizontal gestures to the browser.
    if (committed && !event.defaultPrevented && !event.ctrlKey && !event.metaKey &&
        Number.isFinite(event.deltaY) && event.deltaY !== 0 && Math.abs(event.deltaY) >= Math.abs(event.deltaX || 0)) {
      event.preventDefault(); void scroll(event.deltaY > 0 ? 3 : -3);
    }
  };
  const key = event => {
    if (event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.isComposing) return;
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
    reset() {
      pendingTransaction?.discard(); ++serial; localAbort?.abort(); committed = null;
      engine.reset(); loaded.clear();
      presentationQueue.finally(() => { if (!freed) releaseOutput(); }).catch(() => {});
    },
    free() {
      if (freed) return;
      pendingTransaction?.discard();
      freed = true; ++serial; localAbort?.abort();
      canvas.removeEventListener('wheel', wheel); canvas.removeEventListener('keydown', key);
      canvas.remove(); canvas.width = 0; canvas.height = 0;
      engine.free(); presentationQueue.finally(() => { releaseOutput(); staging.remove(); }).catch(() => {}); loaded.clear(); committed = null;
    },
  };
}
