import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { createLocalServer, LocalStorage } from '../local.mjs';
import { validateSnapshot } from '../src/worker.js';

// No inferred path or fabricated semantic fixture. Coordinator supplies real
// encode_binary[_with_resources] output after the Rust build reservation.
const path = process.env.BMWP2_FIXTURE;
test('external Rust BMWP2 full snapshot survives real HTTP relay byte-for-byte', { skip: !path && 'set BMWP2_FIXTURE to Rust-produced binary' }, async () => {
  const bytes = new Uint8Array(await readFile(path));
  validateSnapshot(bytes);
  const key = 'isolated-cross-language-test';
  const env = { CHANNEL: 'main', PUBLISHER_KEY_SHA256: createHash('sha256').update(key).digest('hex') };
  const storage = new LocalStorage();
  const running = await createLocalServer({ env, storage });
  const url = `${running.url}/api/live/main`;
  const auth = { Authorization: `Bearer ${key}` };
  try {
    const started = await fetch(`${url}/publisher/start`, { method: 'POST', headers: { ...auth, 'Content-Type': 'application/json' }, body: JSON.stringify({ protocol: 1, renderer: 'bmwp-2', publisher_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa' }) });
    assert.equal(started.status, 200);
    const { epoch } = await started.json();
    const uploaded = await fetch(`${url}/publisher/frame`, { method: 'PUT', headers: { ...auth, 'X-Bmux-Epoch': epoch, 'X-Bmux-Revision': '1', 'Content-Type': 'application/octet-stream' }, body: bytes });
    assert.equal(uploaded.status, 204);
    const status = await (await fetch(`${url}/status`)).json();
    assert.equal(status.state, 'live'); assert.equal(status.revision, '1');
    const frame = await fetch(`${url}/presentation?epoch=${epoch}`);
    assert.equal(frame.status, 200); assert.equal(frame.headers.get('Content-Type'), 'application/octet-stream');
    assert.equal(frame.headers.get('X-Snapshot-Revision'), '1');
    assert.deepEqual(new Uint8Array(await frame.arrayBuffer()), bytes);
    // Fixture includes arbitrary image payloads, so exact bytes check is vital.
    assert.ok(JSON.stringify(storage.value).length < 1024);
  } finally { await new Promise(r => { running.server.close(r); running.server.closeAllConnections(); }); }
});
