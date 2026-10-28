import test from 'node:test';
import assert from 'node:assert/strict';
import { createDeliveryDiagnostics } from '../browser/live-viewer.js';
import { mountDiagnosticPanel } from '../browser/live-page.js';

// No peer-owned string, even a syntactically valid UUID, is exported verbatim.
test('diagnostic export is allowlisted, detached and bounded', () => {
  let clock = 0;
  const hash = 'a'.repeat(64);
  const d = createDeliveryDiagnostics(() => clock, `https://example.test/live-assets/${hash}/`);
  d.connection('open'); d.source('live');
  const secret = 'raw frame / fence / key / hostile peer';
  d.reason(secret); d.source(secret); d.connection(secret);
  d.submitted({ generation: secret, revision: '18446744073709551615', payload: secret }); d.ack();
  for (let i = 0; i < 10000; i++) { clock++; d.receive(6); }
  const a = d.snapshot();
  assert.equal(a.history.length, 64);
  assert.equal(a.builds.browser, hash);
  assert.equal(a.builds.relay, null);
  assert.equal(a.agesMs.submitted, 10000);
  assert.deepEqual(a.submitted, { generation: 1, revision: '18446744073709551615' });
  assert.ok(!JSON.stringify(a).includes(secret));
  a.counts.receive = -1; a.submitted.revision = '9'; a.history[0].event = secret;
  assert.equal(d.snapshot().counts.receive, 10000);
  assert.equal(d.snapshot().submitted.revision, '18446744073709551615');
  assert.ok(!JSON.stringify(d.snapshot()).includes(secret));
  d.clear(); d.submitted({ generation: 'new secret', revision: '1' });
  assert.equal(d.snapshot().submitted.generation, 2);
  const invalidBuild = createDeliveryDiagnostics(() => 0, `https://evil/?key=${secret}`);
  assert.equal(invalidBuild.snapshot().builds.browser, null);
});

function dom() {
  class Element {
    constructor() { this.children = []; this.listeners = {}; this.style = {}; this.hidden = false; }
    append(...children) { this.children.push(...children); }
    after(node) { this.sibling = node; }
    setAttribute() {}
    addEventListener(name, callback) { this.listeners[name] = callback; }
    remove() { this.removed = true; }
  }
  const root = new Element(); root.ownerDocument = { createElement: () => new Element() };
  return root;
}

test('panel opens a bounded refresh, exports only on demand, and disposes timers', () => {
  const root = dom(), timers = new Map(); let id = 0;
  const d = createDeliveryDiagnostics(() => 0); d.connection('open'); d.source('live');
  const handle = { get diagnostics() { return d.snapshot(); }, exportDiagnostics: () => JSON.stringify(d.snapshot()) };
  const panel = mountDiagnosticPanel({ root, handle,
    setTimer(fn) { timers.set(++id, fn); return id; }, clearTimer(key) { timers.delete(key); } });
  const details = root.sibling;
  const [, output, button, exported] = details.children;
  assert.match(output.textContent, /idle/);
  assert.match(output.textContent, /not proof of ongoing delivery/);
  assert.match(output.textContent, /not physical display/);
  assert.equal(timers.size, 0); assert.equal(exported.hidden, true);
  details.open = true; details.listeners.toggle(); assert.equal(timers.size, 1);
  button.listeners.click(); assert.equal(exported.hidden, false);
  assert.deepEqual(JSON.parse(exported.value), d.snapshot());
  details.open = false; details.listeners.toggle(); assert.equal(timers.size, 0);
  details.open = true; details.listeners.toggle(); panel.dispose();
  assert.equal(timers.size, 0); assert.equal(details.removed, true);
});
