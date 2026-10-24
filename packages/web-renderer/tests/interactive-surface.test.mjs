import test from 'node:test';
import assert from 'node:assert/strict';
import { mountInteractiveSurface } from '../browser/interactive-surface.js';

// Deliberately small DOM mocks: dispatch the actual bridge's event listeners,
// including focus transitions and synchronous lostpointercapture on release.
class Target {
  listeners = new Map();
  addEventListener(type, callback, options) {
    const entries = this.listeners.get(type) ?? [];
    entries.push({ callback, options }); this.listeners.set(type, entries);
  }
  removeEventListener(type, callback) {
    this.listeners.set(type, (this.listeners.get(type) ?? []).filter(e => e.callback !== callback));
  }
  fire(type, properties = {}) {
    const event = { type, target: this, defaultPrevented: false,
      preventDefault() { this.defaultPrevented = true; }, ...properties };
    for (const { callback } of [...(this.listeners.get(type) ?? [])]) callback(event);
    return event;
  }
  get listenerCount() { return [...this.listeners.values()].reduce((n, entries) => n + entries.length, 0); }
}
class Element extends Target {
  attributes = new Map(); style = {}; children = []; value = ''; captures = new Set();
  rect = { left: 10, top: 20, width: 400, height: 240 };
  constructor(document) { super(); this.ownerDocument = document; }
  setAttribute(key, value) { this.attributes.set(key, value); }
  getAttribute(key) { return this.attributes.get(key) ?? null; }
  removeAttribute(key) { this.attributes.delete(key); }
  appendChild(child) { this.children.push(child); child.parent = this; }
  remove() { this.parent.children = this.parent.children.filter(child => child !== this); }
  getBoundingClientRect() { return this.rect; }
  focus() {
    const document = this.ownerDocument, old = document.activeElement;
    if (old === this) return;
    document.activeElement = this;
    old?.fire('blur', { relatedTarget: this });
    this.fire('focus', { relatedTarget: old });
  }
  setPointerCapture(id) { this.captures.add(id); }
  hasPointerCapture(id) { return this.captures.has(id); }
  releasePointerCapture(id) { this.captures.delete(id); this.fire('lostpointercapture', { pointerId: id }); }
}
function fixture(options = {}) {
  const window = new Target(), media = [], observers = [], events = [], sizes = [], points = [];
  window.devicePixelRatio = 2;
  window.matchMedia = query => { const target = new Target(); target.query = query; media.push(target); return target; };
  window.ResizeObserver = class {
    constructor(callback) { this.callback = callback; observers.push(this); }
    observe(target) { this.target = target; }
    disconnect() { this.disconnected = true; }
  };
  const document = { defaultView: window, createElement() { return new Element(document); } };
  const container = new Element(document), canvas = new Element(document);
  container.appendChild(canvas);
  if (options.tabindex) canvas.setAttribute('tabindex', options.tabindex);
  const handle = mountInteractiveSurface({ container, canvas,
    onInput: value => events.push(value), onResize: value => sizes.push(value),
    mapPoint: (point, event) => { points.push(point); return options.mapPoint ? options.mapPoint(point, event) : { x: Math.floor(point.x / 10), y: Math.floor(point.y / 20) }; },
    ...options,
  });
  const input = container.children[1];
  return { window, media, observers, events, sizes, points, document, container, canvas, input, handle };
}
test('real Chromium editing, embed isolation, resize and disposal lifecycle', {
  skip: process.env.BMUX_INTERACTIVE_BROWSER_TEST !== '1' ? 'opt in with BMUX_INTERACTIVE_BROWSER_TEST=1 and CHROMIUM_BIN' : false,
}, async () => {
  const { createServer } = await import('node:http');
  const { readFile, mkdtemp, rm } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  const { spawn } = await import('node:child_process');
  const executable = process.env.CHROMIUM_BIN;
  assert.ok(executable, 'CHROMIUM_BIN must name an explicit Chromium executable');
  const source = await readFile(new URL('../browser/interactive-surface.js', import.meta.url));
  const page = `<!doctype html><meta charset="utf-8"><body><pre id="result">pending</pre>
    <div id="a"><canvas style="width:200px;height:100px"></canvas></div>
    <div id="b"><canvas style="width:200px;height:100px"></canvas></div>
    <input id="outside"><script type="module">
    import {mountInteractiveSurface} from '/bridge.js';
    const check = (value, message) => { if (!value) throw Error(message); };
    try {
      const events = [[], []], sizes = [], writes = [];
      const roots = ['a', 'b'].map(id => document.getElementById(id));
      const handles = roots.map((container, i) => mountInteractiveSurface({
        container, canvas:container.querySelector('canvas'), onInput:e => events[i].push(e),
        mapPoint:p => ({x:Math.floor(p.x/10),y:Math.floor(p.y/20)}),
        onResize:s => sizes.push(s), writeClipboard:async text => writes.push(text)
      }));
      const inputs = roots.map(root => root.querySelector('textarea'));
      handles[0].focus(); check(document.activeElement === inputs[0], 'focus bridge');
      document.execCommand('insertText', false, 'native edit');
      check(events[0].filter(e => e.type === 'text').length === 1, 'exactly once native editing');
      check(events[0].find(e => e.type === 'text').text === 'native edit', 'native text content');
      handles[1].focus(); document.execCommand('insertText', false, 'second');
      check(events[0].filter(e => e.type === 'text').length === 1, 'embed isolation');
      check(events[1].find(e => e.type === 'text').text === 'second', 'second embed text');
      inputs[1].dispatchEvent(new CompositionEvent('compositionstart'));
      inputs[1].dispatchEvent(new KeyboardEvent('keydown', {key:'Enter',code:'Enter',isComposing:true,cancelable:true}));
      inputs[1].dispatchEvent(new CompositionEvent('compositionend', {data:'字'}));
      inputs[1].dispatchEvent(new InputEvent('input', {inputType:'insertFromComposition',data:'字'}));
      check(events[1].filter(e => e.type === 'text' && e.text === '字').length === 1, 'composition echo');
      check(!events[1].some(e => e.type === 'key'), 'composition Enter');
      handles[1].setAccessibility({label:'Committed label'});
      check(inputs[1].getAttribute('aria-label') === 'Committed label', 'committed accessibility');
      handles[1].setSemanticMetadata({regions:[{id:'choice',role:'option',
        accessibility:{label:'Committed choice',text:'Visible source',focused:true,selected:true,disabled:false}}]});
      const semantic = roots[1].querySelector('[data-semantic-id="choice"]');
      check(semantic.getAttribute('role') === 'option' && semantic.getAttribute('aria-selected') === 'true', 'semantic state');
      check(inputs[1].getAttribute('aria-activedescendant') === semantic.id, 'semantic focus');
      handles[1].setSemanticMetadata({regions:[{id:'choice',role:'option',
        accessibility:{label:'secret',text:'secret',sensitive:true}}]});
      check(!roots[1].querySelector('[data-bmux-semantics]').textContent.includes('secret'), 'sensitive text suppression');
      check(!roots[1].querySelector('[data-semantic-id="choice"]').hasAttribute('aria-label'), 'sensitive label suppression');
      await handles[1].writeClipboard('source'); check(writes.join() === 'source', 'explicit clipboard');
      roots[1].querySelector('canvas').style.width = '300px';
      await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      check(sizes.some(s => s.width === 300), 'ResizeObserver: ' + JSON.stringify(sizes));
      document.getElementById('outside').focus();
      check(events[1].at(-1).type === 'focus' && !events[1].at(-1).focused, 'outside focus');
      handles.forEach(h => {h.dispose(); h.dispose();});
      check(!document.querySelector('textarea'), 'dispose input bridges');
      check(!document.querySelector('[data-bmux-semantics]'), 'dispose semantics');
      const count = events[1].length;
      inputs[1].dispatchEvent(new InputEvent('input', {inputType:'insertText',data:'late'}));
      check(events[1].length === count, 'dispose listeners');
      document.getElementById('result').textContent = 'BRIDGE_BROWSER_PASS';
      await fetch('/result', {method:'POST', body:'BRIDGE_BROWSER_PASS'});
    } catch(error) {
      document.getElementById('result').textContent = 'BRIDGE_BROWSER_FAIL: '+error.stack;
      await fetch('/result', {method:'POST', body:'BRIDGE_BROWSER_FAIL: '+error.stack});
    }
    </script>`;
  let report;
  const reported = new Promise(resolve => { report = resolve; });
  const server = createServer((request, response) => {
    if (request.url === '/result' && request.method === 'POST') {
      let body = '';
      request.on('data', chunk => { body += chunk; });
      request.on('end', () => { response.end('ok'); report(body); });
      return;
    }
    response.setHeader('Content-Type', request.url === '/bridge.js' ? 'text/javascript' : 'text/html');
    response.end(request.url === '/bridge.js' ? source : page);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const profile = await mkdtemp(join(tmpdir(), 'bmux-input-browser-'));
  let child, exit;
  try {
    const output = await new Promise((resolve, reject) => {
      child = spawn(executable, ['--headless', '--disable-gpu', '--no-first-run',
        '--no-default-browser-check', '--user-data-dir=' + profile,
        'http://127.0.0.1:' + server.address().port]);
      let stderr = '';
      const timer = setTimeout(() => reject(Error('Chromium timed out: '+stderr)), 20000);
      child.stdout.resume();
      child.stderr.on('data', data => { stderr = (stderr + data).slice(-4000); });
      child.on('error', error => { clearTimeout(timer); reject(error); });
      exit = new Promise(done => child.on('close', code => {
        clearTimeout(timer); done();
        reject(Error('Chromium exited before reporting '+code+': '+stderr));
      }));
      reported.then(value => { clearTimeout(timer); resolve(value); });
    });
    assert.equal(output, 'BRIDGE_BROWSER_PASS');
  } finally {
    child?.kill();
    await exit;
    await new Promise(resolve => server.close(resolve));
    await rm(profile, { recursive: true, force: true });
  }
});

const keys = properties => ({ key: 'a', code: 'KeyA', ...properties });
const pointer = properties => ({ pointerId: 1, clientX: 35, clientY: 65, button: 0, buttons: 1, ...properties });
const commands = f => f.events.filter(event => event.type !== 'focus');

test('normal text is emitted once from input, never printable keydown/beforeinput', () => {
  const f = fixture(); f.handle.focus();
  const key = f.input.fire('keydown', keys());
  const before = f.input.fire('beforeinput', { inputType: 'insertText', data: 'a' });
  assert.equal(key.defaultPrevented, false); assert.equal(before.defaultPrevented, false);
  f.input.value = 'a'; f.input.fire('input', { inputType: 'insertText', data: 'a' });
  assert.deepEqual(commands(f), [{ type: 'text', text: 'a' }]);
  assert.equal(f.input.value, '');
  f.input.fire('input', { inputType: 'insertText', data: '👩🏽‍💻' });
  assert.equal(commands(f)[1].text, '👩🏽‍💻'); f.handle.dispose();
});

test('IME intermediate text and Enter never submit; both final browser event orders commit once', () => {
  for (const finalBeforeEnd of [false, true]) {
    const f = fixture();
    f.input.fire('compositionstart');
    f.input.fire('beforeinput', { inputType: 'insertCompositionText', data: 'に', isComposing: true });
    f.input.fire('input', { inputType: 'insertCompositionText', data: 'に', isComposing: true });
    f.input.fire('keydown', keys({ key: 'Enter', code: 'Enter', isComposing: true }));
    if (finalBeforeEnd) f.input.fire('input', { inputType: 'insertText', data: '日本' });
    f.input.fire('compositionend', { data: '日本' });
    if (!finalBeforeEnd) {
      f.input.value = '日本';
      f.input.fire('beforeinput', { inputType: 'insertFromComposition', data: '日本' });
      f.input.fire('input', { inputType: 'insertFromComposition', data: '日本' });
    }
    assert.deepEqual(commands(f), [{ type: 'text', text: '日本' }]);
    f.input.fire('keydown', keys());
    f.input.fire('input', { inputType: 'insertText', data: '日本' });
    assert.equal(commands(f).length, 2); f.handle.dispose();
  }
});

test('cancelled composition emits nothing and null input data uses native editing value', () => {
  const f = fixture(); f.input.fire('compositionstart');
  f.input.fire('compositionend', { data: '' });
  f.input.value = 'mobile'; f.input.fire('input', { inputType: 'insertReplacementText', data: null });
  assert.deepEqual(commands(f), [{ type: 'text', text: 'mobile' }]); f.handle.dispose();
});

test('paste is explicit plain text, prevented and not duplicated by input', () => {
  const f = fixture();
  const paste = f.input.fire('paste', { clipboardData: { getData(type) { assert.equal(type, 'text/plain'); return 'a\nb'; } } });
  assert.equal(paste.defaultPrevented, true);
  f.input.fire('beforeinput', { inputType: 'insertFromPaste', data: null });
  f.input.value = 'a\nb'; f.input.fire('input', { inputType: 'insertFromPaste', data: null });
  assert.deepEqual(commands(f), [{ type: 'paste', text: 'a\nb' }]); f.handle.dispose();
});

test('captured keys preserve modifiers/repeat; browser shortcuts and unrelated targets remain unowned', () => {
  const f = fixture();
  assert.equal(f.input.fire('keydown', keys({ key: 'Tab', code: 'Tab', shiftKey: true, repeat: true })).defaultPrevented, true);
  assert.deepEqual(commands(f), [{ type: 'key', key: 'Tab', code: 'Tab', ctrl: false, alt: false, shift: true, meta: false, repeat: true }]);
  assert.equal(f.input.fire('keydown', keys({ ctrlKey: true })).defaultPrevented, false);
  const outside = new Element(f.document);
  assert.equal(f.canvas.fire('keydown', { ...keys({ key: 'Enter' }), target: outside }).defaultPrevented, false);
  outside.fire('paste', { clipboardData: { getData: () => 'outside' } });
  assert.equal(commands(f).length, 1); f.handle.dispose();
  const custom = fixture({ isCapturedKey: event => event.ctrlKey && event.key === 's' });
  assert.equal(custom.input.fire('keydown', keys({ key: 's', ctrlKey: true })).defaultPrevented, true);
  assert.equal(commands(custom)[0].ctrl, true); custom.handle.dispose();
});

test('mapping uses CSS pixels, captures pointer, and releases without duplicate cancellation', () => {
  const f = fixture();
  assert.equal(f.canvas.fire('pointerdown', pointer({ ctrlKey: true })).defaultPrevented, true);
  assert.deepEqual(f.points[0], { x: 25, y: 45 });
  assert.equal(f.canvas.captures.has(1), true);
  assert.deepEqual(commands(f)[0], { type: 'pointer', action: 'down', x: 2, y: 2, button: 0, buttons: 1, ctrl: true, alt: false, shift: false, meta: false });
  f.canvas.fire('pointermove', pointer({ clientX: 65 }));
  f.canvas.fire('pointerup', pointer({ buttons: 0 }));
  assert.deepEqual(commands(f).map(e => e.action), ['down', 'move', 'up']);
  assert.equal(f.canvas.captures.size, 0); f.handle.dispose();
});

test('pointer cancel, lost capture, blur, and disposal each cancel active pointers exactly once', () => {
  for (const ending of ['cancel', 'lost', 'blur', 'dispose']) {
    const f = fixture(); f.canvas.fire('pointerdown', pointer());
    if (ending === 'cancel') f.canvas.fire('pointercancel', pointer({ buttons: 0 }));
    if (ending === 'lost') f.canvas.releasePointerCapture(1);
    if (ending === 'blur') f.window.fire('blur');
    f.handle.dispose(); f.handle.dispose();
    assert.deepEqual(commands(f).map(e => e.action), ['down', 'cancel'], ending);
    assert.equal(f.canvas.captures.size, 0);
  }
});

test('unmapped points preserve browser defaults; wheel carries raw deltas and modifiers', () => {
  const f = fixture({ mapPoint: p => p.x < 0 ? null : { x: 7, y: 8 } });
  assert.equal(f.canvas.fire('pointerdown', pointer({ clientX: 0 })).defaultPrevented, false);
  assert.equal(f.canvas.fire('wheel', pointer({ clientX: 0 })).defaultPrevented, false);
  const wheel = f.canvas.fire('wheel', pointer({ deltaX: 1.5, deltaY: -3, deltaMode: 1, altKey: true, metaKey: true }));
  assert.equal(wheel.defaultPrevented, true);
  assert.deepEqual(commands(f), [{ type: 'wheel', x: 7, y: 8, delta_x: 1.5, delta_y: -3, delta_mode: 1, ctrl: false, alt: true, shift: false, meta: true }]);
  assert.equal(f.canvas.listeners.get('wheel')[0].options.passive, false); f.handle.dispose();
});

test('two embeds have independent focus, IME, and keyboard listeners', () => {
  const a = fixture(), b = fixture(); a.handle.focus(); b.handle.focus();
  a.input.fire('compositionstart');
  b.input.fire('keydown', keys({ key: 'Enter', code: 'Enter' }));
  a.input.fire('keydown', keys({ key: 'Enter', code: 'Enter' }));
  assert.equal(commands(a).length, 0); assert.equal(commands(b).length, 1);
  a.handle.dispose(); b.handle.dispose();
});

test('resize and DPR changes notify once, without mutating canvas pixels', () => {
  const f = fixture();
  assert.deepEqual(f.sizes, [{ width: 400, height: 240, device_pixel_ratio: 2 }]);
  f.handle.resize(); f.observers[0].callback(); assert.equal(f.sizes.length, 1);
  f.canvas.rect.width = 600; f.observers[0].callback(); assert.equal(f.sizes.length, 2);
  f.window.devicePixelRatio = 3; f.media[0].fire('change');
  assert.deepEqual(f.sizes[2], { width: 600, height: 240, device_pixel_ratio: 3 });
  assert.equal(f.media[0].listenerCount, 0); assert.equal(f.media[1].listenerCount, 1);
  assert.equal(f.canvas.width, undefined); f.handle.dispose();
});

test('accessibility uses only supplied committed strings; clipboard requires explicit callback', async () => {
  const writes = [], f = fixture({ writeClipboard: async text => writes.push(text) });
  f.handle.setAccessibility({ label: 'Committed editor', description: 'Rust description' });
  assert.equal(f.input.getAttribute('aria-label'), 'Committed editor');
  assert.equal(f.input.getAttribute('aria-description'), 'Rust description');
  assert.equal(f.input.getAttribute('aria-hidden'), null);
  f.handle.setAccessibility(); assert.equal(f.input.getAttribute('aria-label'), null);
  await f.handle.writeClipboard('selected source'); assert.deepEqual(writes, ['selected source']);
  f.handle.dispose(); await assert.rejects(f.handle.writeClipboard('late'), /disposed/);
  const noClipboard = fixture();
  await assert.rejects(noClipboard.handle.writeClipboard('no ambient access'), /callback/);
  noClipboard.handle.dispose();
});

test('virtual keyboard edits become command keys; fallback native paste keeps its type', () => {
  const f = fixture();
  for (const [inputType, key] of [['deleteContentBackward', 'Backspace'], ['deleteContentForward', 'Delete'], ['insertLineBreak', 'Enter']]) {
    const event = f.input.fire('beforeinput', { inputType });
    assert.equal(event.defaultPrevented, true);
    assert.equal(commands(f).at(-1).key, key);
  }
  f.input.value = 'native paste';
  f.input.fire('input', { inputType: 'insertFromPaste', data: null });
  assert.deepEqual(commands(f).at(-1), { type: 'paste', text: 'native paste' });
  f.handle.dispose();
});

test('capture policy cannot duplicate printable or AltGraph text', () => {
  const f = fixture({ isCapturedKey: () => true });
  assert.equal(f.input.fire('keydown', keys()).defaultPrevented, false);
  assert.equal(f.input.fire('keydown', keys({ key: '@', ctrlKey: true, altKey: true,
    getModifierState: name => name === 'AltGraph' })).defaultPrevented, false);
  f.input.fire('input', { inputType: 'insertText', data: '@' });
  assert.deepEqual(commands(f), [{ type: 'text', text: '@' }]); f.handle.dispose();
});

test('IME final input preceding compositionend does not suppress next identical mobile text', () => {
  const f = fixture(); f.input.fire('compositionstart');
  f.input.fire('input', { inputType: 'insertText', data: '字', isComposing: false });
  f.input.fire('compositionend', { data: '字' });
  f.input.fire('beforeinput', { inputType: 'insertText', data: '字' });
  f.input.fire('input', { inputType: 'insertText', data: '字' });
  assert.deepEqual(commands(f), [{ type: 'text', text: '字' }, { type: 'text', text: '字' }]);
  f.handle.dispose();
});

test('window refocus restores input focus state without broadcasting input', () => {
  const f = fixture(); f.handle.focus(); f.window.fire('blur'); f.window.fire('focus');
  assert.deepEqual(f.events, [{ type: 'focus', focused: true }, { type: 'focus', focused: false }, { type: 'focus', focused: true }]);
  f.handle.dispose();
});

test('captured drag cancellation still releases when mapping stops returning cells', () => {
  let mapped = true;
  const f = fixture({ mapPoint: () => mapped ? { x: 1, y: 2 } : null });
  f.canvas.fire('pointerdown', pointer()); mapped = false;
  f.canvas.fire('pointercancel', pointer({ buttons: 0 }));
  assert.equal(f.canvas.captures.size, 0);
  assert.deepEqual(commands(f).map(e => e.action), ['down', 'cancel']); f.handle.dispose();
});

test('two embeds in one document isolate focus and cancel stale IME completion on blur', () => {
  const a = fixture(), events = [];
  const container = new Element(a.document), canvas = new Element(a.document);
  container.appendChild(canvas);
  const b = mountInteractiveSurface({ container, canvas, onInput: event => events.push(event),
    mapPoint: () => ({ x: 0, y: 0 }) });
  a.handle.focus(); a.input.fire('compositionstart'); b.focus();
  a.input.fire('compositionend', { data: 'stale' });
  container.children[1].fire('input', { inputType: 'insertText', data: 'b' });
  assert.equal(a.events.at(-1).focused, false);
  assert.deepEqual(commands(a), []);
  assert.deepEqual(events, [{ type: 'focus', focused: true }, { type: 'text', text: 'b' }]);
  a.handle.dispose(); b.dispose();
});

test('null-data final IME beforeinput does not lose deduplication', () => {
  const f = fixture(); f.input.fire('compositionstart');
  f.input.fire('compositionend', { data: '字' });
  f.input.fire('beforeinput', { inputType: 'insertText', data: null });
  f.input.value = '字'; f.input.fire('input', { inputType: 'insertText', data: null });
  assert.deepEqual(commands(f), [{ type: 'text', text: '字' }]); f.handle.dispose();
});

test('semantic projection preserves producer IDs/order/state and suppresses sensitive content', () => {
  const f = fixture(), g = fixture();
  const scene = { regions: [
    { id: 'source/β', role: 'option', area: { x: 1, y: 2 }, accessibility: {
      label: 'Choice', description: 'Description', text: 'Logical source',
      selected: true, disabled: false, focused: true, actions: ['activate'] } },
    { id: 'secret', role: 'textbox', accessibility: {
      label: 'secret label', description: 'secret description', text: 'secret text', sensitive: true } },
  ] };
  f.handle.setSemanticMetadata(scene); g.handle.setSemanticMetadata(scene);
  const host = f.container.children[2], [choice, secret] = host.children;
  assert.equal(choice.getAttribute('data-semantic-id'), 'source/β');
  assert.equal(choice.getAttribute('role'), 'option');
  assert.equal(choice.getAttribute('aria-selected'), 'true');
  assert.equal(choice.getAttribute('aria-disabled'), 'false');
  assert.equal(choice.textContent, 'Logical source');
  assert.equal(f.input.getAttribute('aria-activedescendant'), choice.getAttribute('id'));
  assert.notEqual(choice.getAttribute('id'), g.container.children[2].children[0].getAttribute('id'));
  for (const attribute of ['aria-label', 'aria-description']) assert.equal(secret.getAttribute(attribute), null);
  assert.equal(secret.textContent, undefined);
  const oldID = choice.getAttribute('id');
  f.handle.setSemanticMetadata({ regions: [scene.regions[0]] });
  assert.equal(f.container.children.length, 3);
  assert.equal(f.container.children[2].children[0].getAttribute('id'), oldID);
  assert.throws(() => f.handle.setSemanticMetadata({regions:[scene.regions[0], scene.regions[0]]}), /unique/);
  assert.equal(f.container.children[2].children.length, 1);
  f.handle.setSemanticMetadata();
  assert.equal(f.input.getAttribute('aria-activedescendant'), null);
  f.handle.dispose(); g.handle.dispose();
  assert.equal(f.container.children.length, 1);
  f.handle.setSemanticMetadata(scene); assert.equal(f.container.children.length, 1);
});

test('focus transitions are deduplicated; teardown removes every owned resource and restores tabindex', () => {
  const f = fixture({ tabindex: '7' }); f.canvas.focus();
  assert.equal(f.document.activeElement, f.input);
  assert.deepEqual(f.events, [{ type: 'focus', focused: true }]);
  const outside = new Element(f.document); outside.focus();
  assert.deepEqual(f.events.at(-1), { type: 'focus', focused: false });
  f.handle.dispose(); const count = f.events.length;
  for (const target of [f.canvas, f.input, f.window, ...f.media]) assert.equal(target.listenerCount, 0);
  assert.equal(f.container.children.length, 1); assert.equal(f.observers[0].disconnected, true);
  assert.equal(f.canvas.getAttribute('tabindex'), '7');
  f.input.fire('input', { inputType: 'insertText', data: 'late' });
  f.handle.focus(); f.handle.resize(); assert.equal(f.events.length, count);
  const unset = fixture(); unset.handle.dispose(); assert.equal(unset.canvas.getAttribute('tabindex'), null);
});
