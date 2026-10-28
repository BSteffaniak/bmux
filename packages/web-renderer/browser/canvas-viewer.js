// Mountable output-only Canvas adapter. Rust remains the presentation authority.
let nextId = 0;
export async function mountCanvasViewer({ container, assetBase, loadModule = url => import(url) }) {
  const base = new URL(assetBase, container.ownerDocument.baseURI);
  const module = await loadModule(new URL('bmux_web_renderer.js', base).href);
  await module.default(new URL('bmux_web_renderer_bg.wasm', base).href);
  let generation = 0;
  let disposed = false;
  let pending = false;
  let retained = null;
  let visible = null;
  let identity = null;
  function release(state) {
    if (!state || state.freed) return;
    state.freed = true;
    state.canvas.remove();
    state.renderer.free();
  }
  function clear() {
    generation++;
    identity = null;
    visible?.remove();
    visible = null;
    const previous = retained;
    retained = null;
    previous?.canvas.remove(); // Pending WASM writes only touch a detached target.
    if (!pending) release(previous);
  }
  function staging() {
    const canvas = container.ownerDocument.createElement('canvas');
    canvas.id = `bmux-audience-staging-${++nextId}`;
    canvas.hidden = true;
    canvas.style.cssText = 'display:none';
    container.append(canvas); // CanvasRenderer resolves the explicit element ID.
    let renderer;
    try {
      renderer = new module.CanvasRenderer(canvas.id, 10, 20);
      renderer.set_metrics(10, 20, '16px monospace', 16);
      return {canvas, renderer, freed:false};
    } catch (error) {
      canvas.remove(); renderer?.free(); throw error;
    }
  }
  return {
    capabilities: ['bmwp-delta-v1'],
    get identity() { return identity && { ...identity }; },
    clear,
    async present(bytes, admitted, onDecoded = () => {}) {
      if (disposed || pending) return false;
      pending = true;
      const ticket = generation;
      let state;
      let next = null;
      let accepted = false;
      try {
        state = retained ?? staging();
        retained = state;
        const inspection = JSON.parse(state.renderer.inspect_binary(bytes));
        if (!inspection.supported) throw new Error('unsupported presentation');
        await state.renderer.present_binary(bytes);
        onDecoded();
        if (disposed || ticket !== generation || !admitted()) return false;
        // Retain the decoder/delta base on a hidden canvas. Only expose a copy
        // after admission; a pending update must never alter visible output.
        next = container.ownerDocument.createElement('canvas');
        next.hidden = true;
        next.width = state.canvas.width;
        next.height = state.canvas.height;
        next.style.cssText = 'max-width:100%;height:auto;pointer-events:none';
        next.setAttribute('aria-label', 'Read-only live terminal output');
        const context = next.getContext('2d');
        if (!context) throw new Error('Canvas output context unavailable');
        context.drawImage(state.canvas, 0, 0);
        if (disposed || ticket !== generation || !admitted()) return false;
        container.append(next);
        visible?.remove();
        visible = next;
        visible.hidden = false;
        identity = { generation: inspection.generation, revision: inspection.revision };
        accepted = true;
        return true;
      } finally {
        pending = false;
        if (!accepted) {
          next?.remove();
          if (retained === state) retained = null;
          release(state); // An unadmitted decoder state cannot be a delta base.
        }
      }
    },
    dispose() { disposed = true; clear(); },
  };
}
