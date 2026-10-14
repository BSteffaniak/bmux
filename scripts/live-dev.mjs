#!/usr/bin/env node
// Same Worker handler as production, with loopback same-origin viewer assets.
import http from 'node:http';
import { readFile, realpath } from 'node:fs/promises';
import { resolve, sep } from 'node:path';
import { createLocalServer, LocalStorage } from '../infra/live-relay/local.mjs';

const options = new Map();
for (let i = 2; i < process.argv.length; i += 2) {
  if (!['--assets', '--key-file', '--state-file', '--ready-file'].includes(process.argv[i]) || !process.argv[i + 1])
    throw new Error('Use --assets DIRECTORY --key-file PRIVATE_FILE --state-file PRIVATE_FILE --ready-file FILE');
  options.set(process.argv[i], process.argv[i + 1]);
}
for (const key of ['--assets', '--key-file', '--state-file', '--ready-file'])
  if (!options.has(key)) throw new Error('Missing explicit isolated path');
const { open, writeFile } = await import('node:fs/promises');
const { constants } = await import('node:fs');
const keyHandle = await open(options.get('--key-file'), constants.O_RDONLY | constants.O_NOFOLLOW);
let key;
try {
  const info = await keyHandle.stat();
  if (!info.isFile() || info.uid !== process.getuid() || (info.mode & 0o077)) throw new Error('Key file must be owned and private');
  if (info.size > 4097) throw new Error('Key too large');
  key = (await keyHandle.readFile('utf8')).trim();
  if (!/^[!-~]{32,4096}$/.test(key)) throw new Error('Invalid private key');
} finally { await keyHandle.close(); }
const { createHash } = await import('node:crypto');
const relay = await createLocalServer({ port: 0, env: {
  CHANNEL: 'main', PUBLISHER_KEY_SHA256: createHash('sha256').update(key).digest('hex'),
}, storage: new LocalStorage(options.get('--state-file')) });
key = undefined;
const assets = await realpath(options.get('--assets'));
const manifest = JSON.parse(await readFile(resolve(assets, 'manifest.json'), 'utf8'));
const version = manifest.bundle_sha256;
if (!/^[0-9a-f]{64}$/.test(version) || manifest.protocol !== 1 || manifest.renderer !== 'bmwp-2') throw new Error('Use a real generated immutable viewer bundle');
const assetBase = `/live-assets/${version}/`;
const types = new Map([['js', 'text/javascript'], ['wasm', 'application/wasm'], ['css', 'text/css'], ['html', 'text/html']]);
let active = 0;
const viewer = http.createServer(async (request, response) => {
  if (++active > 32) { active--; response.writeHead(503); response.end(); return; }
  try {
    const url = new URL(request.url, 'http://localhost');
    if (url.pathname.startsWith('/api/live/')) {
      const headers = { ...request.headers };
      delete headers.host;
      const upstream = http.request(relay.url + request.url, { method: request.method, headers }, incoming => {
        response.writeHead(incoming.statusCode, incoming.headers);
        incoming.pipe(response);
      });
      upstream.setTimeout(5000, () => upstream.destroy());
      upstream.on('error', () => { if (!response.headersSent) response.writeHead(503); response.end(); });
      request.on('aborted', () => upstream.destroy());
      response.on('close', () => upstream.destroy());
      request.pipe(upstream);
      return;
    }
    if (request.method !== 'GET' || !['/live', '/live/', '/'].includes(url.pathname) && !url.pathname.startsWith(assetBase)) {
      response.writeHead(404); response.end(); return;
    }
    if (['/live', '/live/', '/'].includes(url.pathname)) {
      response.writeHead(200, { 'Content-Type': 'text/html', 'Cache-Control': 'no-store' });
      response.end(`<!doctype html><meta charset="utf-8"><title>BMUX Live</title><p id="live-status">Offline</p><div id="live-broadcast"></div><script type="module" src="${assetBase}live-page.js"></script>`);
      return;
    }
    const relative = decodeURIComponent(url.pathname.slice(assetBase.length));
    const path = await realpath(resolve(assets, relative));
    if (!path.startsWith(assets + sep)) throw new Error('Invalid path');
    const info = await import('node:fs/promises').then(fs => fs.stat(path));
    if (!info.isFile() || info.size > 16 * 1024 * 1024) throw new Error('Invalid asset');
    response.writeHead(200, { 'Content-Type': types.get(path.split('.').pop()) || 'application/octet-stream', 'Cache-Control': 'no-store' });
    response.end(await readFile(path));
  } catch {
    if (!response.headersSent) response.writeHead(404);
    response.end();
  } finally { if (!response.writableEnded) response.once('close', () => active--); else active--; }
});
viewer.maxConnections = 32;
viewer.requestTimeout = 5000;
viewer.headersTimeout = 5000;
await new Promise(ok => viewer.listen(0, '127.0.0.1', ok));
const url = `http://127.0.0.1:${viewer.address().port}`;
await writeFile(options.get('--ready-file'), JSON.stringify({ url, protocol: 1, renderer: 'bmwp-2' }), { mode: 0o600, flag: 'wx' });
console.log(`Isolated live viewer: ${url}/live/`);
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => {
  viewer.close(); viewer.closeAllConnections(); relay.server.close(); relay.server.closeAllConnections();
});
