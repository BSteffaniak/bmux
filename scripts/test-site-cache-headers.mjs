import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { generateHeaders } from './site-cache-headers.mjs';

const names = ['canvas-viewer.js', 'live-viewer.js', 'live-page.js',
  'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm'];
const sha = data => createHash('sha256').update(data).digest('hex');
async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'bmux-cache-test-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}
async function put(root, path, contents = 'mutable') {
  const full = join(root, path);
  await mkdir(join(full, '..'), { recursive: true });
  await writeFile(full, contents);
}
async function bundle(root, change = '') {
  const contents = names.map(name => name + change);
  const hash = sha(contents.map(bytes => sha(bytes) + '\n').join(''));
  for (let i = 0; i < names.length; i++) await put(root, `live-assets/${hash}/${names[i]}`, contents[i]);
  return hash;
}

test('only exact hashed assets are immutable; mutable pointers and HTML are untouched', async t => {
  const root = await fixture(t);
  const hash = await bundle(root);
  for (const path of ['index.html', 'live.html', 'live-assets/current-version',
    'live-assets/live-page.js', `live-assets/${hash}/extra.js`, `live-assets/${hash}/manifest.json`,
    'js/hyperchad.js', 'js/hyperchad-unhashed.min.js']) await put(root, path);
  await put(root, 'js/hyperchad-0123456789.min.js', 'runtime');
  await generateHeaders(root);
  const headers = await readFile(join(root, '_headers'), 'utf8');
  const urls = headers.split('\n').filter(line => line.startsWith('/'));
  assert.deepEqual(urls, ['/live', '/live.html', '/live-assets/current-version',
    '/live-assets/*/manifest.json', '/js/hyperchad-0123456789.min.js',
    ...names.map(name => `/live-assets/${hash}/${name}`).sort()]);
  assert.equal(headers.match(/Cache-Control: public, max-age=31536000, immutable/g).length, 6);
  assert.equal(headers.match(/Cache-Control: no-store/g).length, 4);
  assert.ok(!headers.includes('/live-assets/*\n'));
  assert.equal(await readFile(join(root, 'index.html'), 'utf8'), 'mutable');
});

test('changed bundle bytes yield different immutable URLs', async t => {
  const root = await fixture(t);
  const first = await bundle(root);
  const second = await bundle(root, 'changed');
  assert.notEqual(first, second);
  await generateHeaders(root);
  const headers = await readFile(join(root, '_headers'), 'utf8');
  assert.ok(headers.includes(`/${first}/live-page.js`));
  assert.ok(headers.includes(`/${second}/live-page.js`));
});

test('reject corrupted bundles and preserve an existing declarative policy', async t => {
  const root = await fixture(t);
  const hash = await bundle(root);
  await put(root, `live-assets/${hash}/live-page.js`, 'corrupted');
  await assert.rejects(generateHeaders(root), /identity mismatch/);
  await rm(join(root, 'live-assets'), { recursive: true });
  await put(root, '_headers', '# existing policy\n');
  await assert.rejects(generateHeaders(root), { code: 'EEXIST' });
  assert.equal(await readFile(join(root, '_headers'), 'utf8'), '# existing policy\n');
});

test('reject policy exceeding Pages rule limit', async t => {
  const root = await fixture(t);
  for (let i = 0; i < 101; i++) await put(root, `js/hyperchad-${i.toString(16).padStart(10, '0')}.js`);
  await assert.rejects(generateHeaders(root), /rule limit/);
});
