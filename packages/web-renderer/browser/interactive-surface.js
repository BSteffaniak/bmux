/**
 * Collect browser input for a caller-owned Rust/WASM surface. No layout, hit
 * testing, clipboard authority, or application state lives here.
 *
 * mapPoint({x, y}, event) maps canvas-local CSS pixels to committed cell
 * coordinates, or returns null outside the authoritative surface. onResize
 * receives {width, height, device_pixel_ratio}; it does not resize the canvas.
 * isCapturedKey(event) selects command keys (not printable text). Default
 * capture is navigation/editing keys without platform shortcut modifiers.
 * Accessibility strings must come from the caller's committed metadata.
 * setSemanticMetadata({regions}) accepts committed SemanticScene JSON; it
 * projects explicit accessibility text/state only, never cells or geometry.
 * Semantic actions are descriptive only: application action routing remains
 * caller-owned. Call only after successful presentation, including on resize.
 */
let nextSurfaceId = 0;

export function mountInteractiveSurface({
  container, canvas, onInput, mapPoint, onResize = () => {},
  isCapturedKey = defaultCapturedKey, writeClipboard,
}) {
  if (!container || !canvas || typeof onInput !== 'function' || typeof mapPoint !== 'function') {
    throw new TypeError('container, canvas, onInput and mapPoint are required');
  }
  const document = container.ownerDocument;
  const window = document.defaultView;
  const input = document.createElement('textarea');
  input.setAttribute('data-bmux-input', '');
  input.setAttribute('autocomplete', 'off');
  input.setAttribute('autocapitalize', 'off');
  input.setAttribute('spellcheck', 'false');
  // Keep a real focusable editing host for mobile keyboards and IME. Never
  // display:none/aria-hidden: both break native editing/accessibility bridges.
  Object.assign(input.style, {
    position: 'absolute', width: '1px', height: '1px', opacity: '0',
    overflow: 'hidden', padding: '0', border: '0', pointerEvents: 'none',
  });
  container.appendChild(input);
  const oldTabIndex = canvas.getAttribute('tabindex');
  if (oldTabIndex === null) canvas.setAttribute('tabindex', '0');
  const removers = [];
  const pointers = new Map();
  const semanticPrefix = `bmux-semantic-${++nextSurfaceId}-`;
  let semanticHost = null;
  // This is an accessibility-only projection of one committed Rust scene, not
  // another layout or action router. IDs/order/text/state are producer-owned.
  function setSemanticMetadata({ regions = [] } = {}) {
    if (disposed) return;
    if (!Array.isArray(regions) || regions.length > 4096 ||
        regions.some(region => typeof region.id !== 'string') ||
        new Set(regions.map(region => region.id)).size !== regions.length) {
      throw new TypeError('semantic regions require bounded unique string IDs');
    }
    const host = document.createElement('div');
    host.setAttribute('data-bmux-semantics', '');
    Object.assign(host.style, { position: 'absolute', width: '1px', height: '1px',
      overflow: 'hidden', clipPath: 'inset(50%)', pointerEvents: 'none' });
    let active = null;
    for (const region of regions) {
      const metadata = region.accessibility;
      if (!metadata) continue;
      const node = document.createElement('div');
      const id = semanticPrefix + encodeURIComponent(region.id);
      node.setAttribute('id', id);
      node.setAttribute('data-semantic-id', region.id);
      // Neutral producer roles need not be ARIA tokens. Do not guess a role
      // from labels or paint; only pass through recognized descriptive roles.
      if (['article', 'document', 'group', 'heading', 'list', 'listitem', 'note',
        'region', 'status', 'log', 'table', 'row', 'cell', 'gridcell', 'option',
        'tab', 'treeitem', 'button', 'textbox', 'checkbox', 'radio', 'link',
        'img', 'progressbar', 'slider', 'separator'].includes(region.role)) {
        node.setAttribute('role', region.role);
      }
      if (!metadata.sensitive) {
        if (metadata.label != null) node.setAttribute('aria-label', metadata.label);
        if (metadata.description != null) node.setAttribute('aria-description', metadata.description);
        if (metadata.text != null) node.textContent = metadata.text;
      }
      if (metadata.disabled != null) node.setAttribute('aria-disabled', String(metadata.disabled));
      if (metadata.selected != null) node.setAttribute('aria-selected', String(metadata.selected));
      if (metadata.focused) active = id;
      host.appendChild(node);
    }
    container.appendChild(host);
    semanticHost?.remove();
    semanticHost = host;
    if (active) input.setAttribute('aria-activedescendant', active);
    else input.removeAttribute('aria-activedescendant');
    input.setAttribute('aria-controls', [...host.children].map(node => node.getAttribute('id')).join(' '));
  }
  let disposed = false, focused = false, composing = false, compositionInputFinished = false;
  let committedComposition = null, pasted = null, lastSize = null, media;
  const emit = value => { if (!disposed) onInput(value); };
  const listen = (target, type, listener, options) => {
    target.addEventListener(type, listener, options);
    removers.push(() => target.removeEventListener(type, listener, options));
  };
  const modifiers = event => ({
    ctrl: Boolean(event.ctrlKey), alt: Boolean(event.altKey),
    shift: Boolean(event.shiftKey), meta: Boolean(event.metaKey),
  });
  const owns = target => target === input || target === canvas;
  function focus() {
    if (!disposed) input.focus({ preventScroll: true });
  }
  function clearEditing() {
    composing = false;
    committedComposition = null;
    pasted = null;
    input.value = '';
  }
  function releasePointers() {
    for (const [id, previous] of pointers) {
      pointers.delete(id);
      emit({ ...previous, action: 'cancel', buttons: 0 });
      if (canvas.hasPointerCapture?.(id)) canvas.releasePointerCapture(id);
    }
  }
  function setFocused(value) {
    if (focused === value) return;
    focused = value;
    if (!value) { clearEditing(); releasePointers(); }
    emit({ type: 'focus', focused: value });
  }
  for (const target of [canvas, input]) {
    listen(target, 'focus', () => setFocused(true));
    listen(target, 'blur', event => {
      if (!owns(event.relatedTarget)) setFocused(false);
    });
    listen(target, 'keydown', event => {
      if (!owns(event.target) || disposed) return;
      // Enter accepts IME candidates; it must never become an app submit.
      if (composing || event.isComposing || event.keyCode === 229) return;
      committedComposition = null;
      pasted = null;
      const printable = Array.from(event.key ?? '').length === 1 &&
        ((!event.ctrlKey && !event.altKey && !event.metaKey) ||
          event.getModifierState?.('AltGraph'));
      if (printable || !isCapturedKey(event)) return;
      event.preventDefault();
      emit({ type: 'key', key: event.key, code: event.code,
        ...modifiers(event), repeat: Boolean(event.repeat) });
    });
    listen(target, 'paste', event => {
      if (!owns(event.target) || !event.clipboardData || composing) return;
      const text = event.clipboardData.getData('text/plain');
      event.preventDefault();
      pasted = text;
      input.value = '';
      emit({ type: 'paste', text });
    });
  }
  listen(canvas, 'focus', focus);
  listen(input, 'compositionstart', () => {
    composing = true; compositionInputFinished = false;
    committedComposition = null; pasted = null;
  });
  listen(input, 'compositionend', event => {
    if (!composing) return; // A late completion after blur/cancellation is stale.
    composing = false;
    const text = event.data ?? input.value;
    committedComposition = compositionInputFinished ? null : text;
    if (text) emit({ type: 'text', text });
    input.value = '';
  });
  listen(input, 'beforeinput', event => {
    // Do not cancel native editing or composition. input is the sole normal
    // text source; beforeinput only delimits possible composition/paste echoes.
    if (event.isComposing || composing) return;
    const editingKey = { deleteContentBackward: 'Backspace', deleteContentForward: 'Delete',
      insertLineBreak: 'Enter', insertParagraph: 'Enter' }[event.inputType];
    if (editingKey) {
      // Virtual keyboards may have no keydown. Physical command keydowns are
      // cancelled above, so browsers do not also perform their native edit.
      event.preventDefault();
      input.value = '';
      emit({ type: 'key', key: editingKey, code: editingKey,
        ...modifiers(event), repeat: false });
      return;
    }
    if (committedComposition !== null && event.data != null &&
        event.data !== committedComposition) {
      committedComposition = null;
    }
    if (event.inputType !== 'insertFromPaste') pasted = null;
  });
  listen(input, 'input', event => {
    if (composing) {
      if (!event.isComposing) compositionInputFinished = true;
      return;
    }
    if (event.isComposing) return;
    const text = event.data ?? input.value;
    const compositionEcho = committedComposition !== null &&
      text === committedComposition &&
      ['insertText', 'insertCompositionText', 'insertFromComposition'].includes(event.inputType);
    const pasteEcho = pasted !== null && event.inputType === 'insertFromPaste';
    committedComposition = null; pasted = null;
    input.value = '';
    // Deletions and line breaks are command-key events, not committed text.
    if (!compositionEcho && !pasteEcho && text &&
        event.inputType?.startsWith('insert') &&
        !['insertLineBreak', 'insertParagraph'].includes(event.inputType)) {
      emit({ type: event.inputType === 'insertFromPaste' ? 'paste' : 'text', text });
    }
  });
  function point(event) {
    const rect = canvas.getBoundingClientRect();
    const mapped = mapPoint({ x: event.clientX - rect.left, y: event.clientY - rect.top }, event);
    return mapped && Number.isFinite(mapped.x) && Number.isFinite(mapped.y) ? mapped : null;
  }
  for (const action of ['down', 'up', 'move', 'cancel']) {
    listen(canvas, `pointer${action}`, event => {
      if (disposed) return;
      const previous = pointers.get(event.pointerId);
      const mapped = point(event) ?? (previous && { x: previous.x, y: previous.y });
      if (!mapped) return;
      const value = { type: 'pointer', action, x: mapped.x, y: mapped.y,
        button: event.button, buttons: event.buttons, ...modifiers(event) };
      if (action === 'down') {
        focus();
        if (disposed) return;
        pointers.set(event.pointerId, value);
        canvas.setPointerCapture(event.pointerId);
      } else if (action === 'move' && previous) pointers.set(event.pointerId, value);
      else if (action === 'up' || action === 'cancel') {
        pointers.delete(event.pointerId);
        if (canvas.hasPointerCapture?.(event.pointerId)) canvas.releasePointerCapture(event.pointerId);
      }
      event.preventDefault();
      emit(value);
    });
  }
  listen(canvas, 'lostpointercapture', event => {
    const previous = pointers.get(event.pointerId);
    if (!previous) return;
    pointers.delete(event.pointerId);
    emit({ ...previous, action: 'cancel', buttons: 0 });
  });
  listen(canvas, 'wheel', event => {
    const mapped = point(event);
    if (!mapped) return;
    event.preventDefault();
    emit({ type: 'wheel', x: mapped.x, y: mapped.y,
      delta_x: event.deltaX, delta_y: event.deltaY, delta_mode: event.deltaMode,
      ...modifiers(event) });
  }, { passive: false });
  listen(window, 'blur', () => setFocused(false));
  listen(window, 'focus', () => {
    if (owns(document.activeElement)) setFocused(true);
  });
  function resize() {
    if (disposed) return;
    const rect = canvas.getBoundingClientRect();
    const size = { width: rect.width, height: rect.height,
      device_pixel_ratio: window.devicePixelRatio || 1 };
    if (lastSize && Object.keys(size).every(key => size[key] === lastSize[key])) return;
    lastSize = size;
    onResize({ ...size });
  }
  function watchDpr() {
    media?.removeEventListener('change', dprChanged);
    media = window.matchMedia?.(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
    media?.addEventListener('change', dprChanged);
  }
  function dprChanged() { if (!disposed) { resize(); if (!disposed) watchDpr(); } }
  listen(window, 'resize', resize);
  const observer = window.ResizeObserver ? new window.ResizeObserver(resize) : null;
  observer?.observe(canvas);
  watchDpr();
  resize();
  return {
    focus, resize, setSemanticMetadata,
    setAccessibility({ label, description } = {}) {
      if (disposed) return;
      for (const [name, value] of [['aria-label', label], ['aria-description', description]]) {
        if (value == null) input.removeAttribute(name);
        else input.setAttribute(name, String(value));
      }
    },
    async writeClipboard(text) {
      if (disposed) throw new Error('surface is disposed');
      if (typeof writeClipboard !== 'function') throw new Error('clipboard write callback is required');
      await writeClipboard(String(text));
    },
    dispose() {
      if (disposed) return;
      setFocused(false);
      releasePointers();
      disposed = true;
      for (const remove of removers) remove();
      observer?.disconnect();
      media?.removeEventListener('change', dprChanged);
      input.remove();
      semanticHost?.remove();
      if (oldTabIndex === null) canvas.removeAttribute('tabindex');
      else canvas.setAttribute('tabindex', oldTabIndex);
    },
  };
}

function defaultCapturedKey(event) {
  return !event.ctrlKey && !event.altKey && !event.metaKey &&
    ['Enter', 'Tab', 'Escape', 'Backspace', 'Delete', 'ArrowLeft', 'ArrowRight',
      'ArrowUp', 'ArrowDown', 'Home', 'End', 'PageUp', 'PageDown'].includes(event.key);
}
