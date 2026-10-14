// Mountable output-only Canvas adapter. Rust remains the presentation authority.
let nextId = 0;
export async function mountCanvasViewer({ container, assetBase, loadModule = url => import(url) }) {
  const base = new URL(assetBase, container.ownerDocument.baseURI);
  const module = await loadModule(new URL('bmux_web_renderer.js', base).href);
  await module.default(new URL('bmux_web_renderer_bg.wasm', base).href);
  let generation = 0;
  let disposed = false;
  let pending = false;
  const canvases = new Set();
  function clear() {
    generation++;
    for (const canvas of canvases) {
      canvas.hidden = true;
      canvas.remove(); // Pending WASM writes can only touch a detached Canvas.
    }
    canvases.clear();
  }
  return {
    clear,
    async present(bytes, admitted) {
      if (disposed || pending) return false;
      pending = true;
      const ticket = generation;
      const canvas = container.ownerDocument.createElement('canvas');
      canvas.id = `bmux-audience-${++nextId}`;
      canvas.hidden = true;
      canvas.style.cssText = 'display:none;max-width:100%;height:auto;pointer-events:none';
      canvas.setAttribute('aria-label', 'Read-only live terminal output');
      canvases.add(canvas);
      container.append(canvas); // CanvasRenderer resolves the explicit element ID.
      let renderer;
      try {
        renderer = new module.CanvasRenderer(canvas.id, 10, 20);
        renderer.set_metrics(10, 20, '16px monospace', 16);
        const inspection = JSON.parse(renderer.inspect_binary(bytes));
        if (!inspection.supported) throw new Error('unsupported presentation');
        await renderer.present_binary(bytes);
        if (disposed || ticket !== generation || !admitted()) return false;
        for (const old of canvases) if (old !== canvas) { old.hidden = true; old.remove(); }
        canvases.clear();
        canvases.add(canvas);
        canvas.style.display = 'block';
        canvas.hidden = false;
        return true;
      } finally {
        renderer?.free();
        pending = false;
        if (canvas.hidden) { canvas.remove(); canvases.delete(canvas); }
      }
    },
    dispose() { disposed = true; clear(); },
  };
}
