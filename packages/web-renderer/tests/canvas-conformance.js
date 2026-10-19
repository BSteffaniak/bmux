// Real-browser conformance helper for the coordinator's headless acceptance.
// Pass validated complete fixtures: first and second share a generation with
// increasing revisions and contain images; replacement has a new generation.
// No DOM mocks: exercises the actual Rust/WASM Canvas backend and ImageBitmaps.
export async function canvasConformance(CanvasRenderer, canvasId, first, second, replacement) {
  const assert = (condition, message) => { if (!condition) throw new Error(message); };
  const canvas = document.getElementById(canvasId);
  const output = canvas.getContext('2d');
  const create = window.createImageBitmap;
  const close = ImageBitmap.prototype.close;
  const live = new Set();
  window.createImageBitmap = async function (...args) {
    const bitmap = await create.apply(this, args);
    live.add(bitmap);
    return bitmap;
  };
  ImageBitmap.prototype.close = function () { live.delete(this); return close.call(this); };
  let freed = false;
  const prototype = CanvasRenderingContext2D.prototype;
  const begin = prototype.beginPath, rect = prototype.rect, clip = prototype.clip;
  const rectangles = new WeakMap();
  let largestClip = 0;
  prototype.beginPath = function (...args) {
    rectangles.set(this, 0);
    return begin.apply(this, args);
  };
  prototype.rect = function (...args) {
    rectangles.set(this, (rectangles.get(this) ?? 0) + 1);
    return rect.apply(this, args);
  };
  prototype.clip = function (...args) {
    largestClip = Math.max(largestClip, rectangles.get(this) ?? 0);
    return clip.apply(this, args);
  };
  const renderer = new CanvasRenderer(canvasId, 10, 20);
  try {
    await renderer.present_json(JSON.stringify(first));
    assert(largestClip === 1, 'full snapshot must use a single clip rectangle, not one per cell');
    assert(live.size > 0, 'first fixture must contain decoded images');
    const metadata = renderer.metadata_json();
    const imageCount = live.size;
    const width = canvas.width;
    const height = canvas.height;
    const pixels = output.getImageData(0, 0, width, height).data.slice();
    const draw = output.drawImage;
    let fail = true;
    output.drawImage = function (...args) {
      if (fail) { fail = false; throw new Error('injected visible-copy failure'); }
      return draw.apply(this, args);
    };
    try {
      let rejected = false;
      try { await renderer.present_json(JSON.stringify(second)); } catch { rejected = true; }
      assert(rejected, 'failed visible copy must reject');
      assert(renderer.metadata_json() === metadata, 'failed copy committed metadata');
      assert(canvas.width === width && canvas.height === height, 'failed copy changed dimensions');
      const repaired = output.getImageData(0, 0, width, height).data;
      assert(pixels.every((value, index) => value === repaired[index]), 'failed copy lost previous output');
      assert(live.size === imageCount, 'failed image staging leaked resources');
    } finally { output.drawImage = draw; }
    renderer.set_metrics(12, 24, '18px monospace', 18);
    assert(renderer.metadata_json() === metadata, 'metric repaint changed scene authority');
    assert(live.size === imageCount, 'metric repaint duplicated resources');
    let invalidMetrics = false;
    try { renderer.set_metrics(Infinity, 24, '18px monospace', 18); }
    catch { invalidMetrics = true; }
    assert(invalidMetrics && renderer.metadata_json() === metadata, 'invalid metrics committed');
    await renderer.present_json(JSON.stringify(second));
    assert(renderer.revision() === String(second.revision), 'retry did not commit');
    let generationRejected = false;
    try { await renderer.present_json(JSON.stringify(replacement)); }
    catch { generationRejected = true; }
    assert(generationRejected, 'generation change requires explicit reset');
    await renderer.repair_json(JSON.stringify(replacement));
    assert(JSON.parse(renderer.metadata_json()).generation === replacement.generation, 'repair failed');
    renderer.reset();
    assert(renderer.metadata_json() === 'null' && live.size === 0, 'reset retained authority/resources');
    await renderer.present_json(JSON.stringify(replacement));
    renderer.free();
    freed = true;
    assert(live.size === 0, 'Drop leaked ImageBitmaps');
  } finally {
    if (!freed) renderer.free();
    prototype.beginPath = begin;
    prototype.rect = rect;
    prototype.clip = clip;
    window.createImageBitmap = create;
    ImageBitmap.prototype.close = close;
  }
}
