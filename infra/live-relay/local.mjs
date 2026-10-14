#!/usr/bin/env node
import http from 'node:http';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { readFile, rename, mkdir, readdir, lstat } from 'node:fs/promises';
import { dirname, resolve, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ChannelHandler, BUDGETS } from './src/worker.js';

// File storage contains only small durable authority metadata; frames stay RAM-only.
export class LocalStorage {
  constructor(path) { this.path = path; this.value = undefined; }
  async get() {
    if (!this.path) return this.value;
    try { return JSON.parse(await readFile(this.path, 'utf8')); }
    catch (e) { if (e.code === 'ENOENT') return undefined; throw e; }
  }
  async put(_key, value) {
    if (this.path) {
      await mkdir(dirname(this.path), { recursive: true, mode: 0o700 });
      const temporary = `${this.path}.${process.pid}.tmp`;
      // fsync the file before rename, then the containing directory before ACK.
      const { open } = await import('node:fs/promises');
      const file = await open(temporary, 'w', 0o600);
      try { await file.writeFile(JSON.stringify(value)); await file.sync(); } finally { await file.close(); }
      await rename(temporary, this.path);
      const directory = await open(dirname(this.path), 'r');
      try { await directory.sync(); } finally { await directory.close(); }
    }
    this.value = structuredClone(value);
  }
}
// Snapshot a strict static allowlist at startup: no symlinks, traversal, arbitrary
// disk reads or SPA fallback on API routes; bounded aggregate asset retention.
async function viewerAssets(directory) {
  const assets = new Map();
  if (!directory) return assets;
  const root = resolve(directory);
  if (!(await lstat(root)).isDirectory() || (await lstat(root)).isSymbolicLink()) throw new Error('invalid viewer directory');
  const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.mjs': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.wasm': 'application/wasm', '.svg': 'image/svg+xml', '.png': 'image/png', '.ico': 'image/x-icon', '.woff2': 'font/woff2' };
  let total = 0, entries = 0;
  async function scan(path, prefix = '') {
    for (const entry of await readdir(path, { withFileTypes: true })) {
      if (++entries > 256) throw new Error('viewer asset entry budget exceeded');
      if (!/^[a-zA-Z0-9_.-]+$/.test(entry.name) || entry.name.startsWith('.') || entry.isSymbolicLink()) throw new Error('unsafe viewer asset');
      const file = resolve(path, entry.name), relative = `${prefix}/${entry.name}`;
      if (entry.isDirectory()) { if (relative.split('/').length > 5) throw new Error('viewer asset depth exceeded'); await scan(file, relative); }
      else if (entry.isFile() && types[extname(entry.name)]) {
        const size = (await lstat(file)).size;
        if (size > 16 * 1024 * 1024 || total + size > 32 * 1024 * 1024) throw new Error('viewer asset byte budget exceeded');
        const bytes = await readFile(file); total += bytes.length;
        assets.set(relative, { bytes, type: types[extname(entry.name)] });
      }
    }
  }
  await scan(root);
  if (assets.has('/index.html')) assets.set('/', assets.get('/index.html'));
  return assets;
}
export async function createLocalServer({ host = '127.0.0.1', port = 0, env = process.env, storage = new LocalStorage(), staticDir } = {}) {
  if (!['127.0.0.1', '::1', 'localhost'].includes(host)) throw new Error('local adapter requires loopback');
  const assets = await viewerAssets(staticDir);
  const handler = new ChannelHandler(storage, env, { local: true });
  await handler.ready;
  let active = 0;
  const server = http.createServer(async (incoming, outgoing) => {
    if (active >= BUDGETS.requests) { outgoing.writeHead(429, { 'Cache-Control': 'no-store', Connection: 'close' }); outgoing.end(); return; }
    active++;
    try {
      const address = server.address();
      const base = `http://${host === '::1' ? '[::1]' : host}:${address.port}`;
      // Do not trust Host or allow absolute-form requests to escape the local base.
      if (!incoming.url.startsWith('/') || incoming.url.startsWith('//')) throw new Error('invalid target');
      const asset = assets.get(incoming.url);
      if (incoming.method === 'GET' && asset && !incoming.url.startsWith('/api/')) {
        outgoing.writeHead(200, { 'Content-Type': asset.type, 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', 'Referrer-Policy': 'no-referrer',
          'Content-Security-Policy': "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'" });
        outgoing.end(asset.bytes); return;
      }
      const abort = new AbortController();
      incoming.on('aborted', () => abort.abort());
      const request = new Request(`${base}${incoming.url}`, {
        method: incoming.method, headers: incoming.headers, signal: abort.signal,
        ...(['GET', 'HEAD'].includes(incoming.method) ? {} : { body: Readable.toWeb(incoming), duplex: 'half' }),
      });
      const response = await handler.fetch(request);
      outgoing.writeHead(response.status, Object.fromEntries(response.headers));
      if (response.body) await pipeline(Readable.fromWeb(response.body), outgoing);
      else outgoing.end();
    } catch {
      if (!outgoing.headersSent) outgoing.writeHead(503, { 'Cache-Control': 'no-store', 'Content-Type': 'application/json' });
      if (!outgoing.destroyed) outgoing.end('{"error":"request rejected"}');
    } finally { active--; }
  });
  server.maxConnections = BUDGETS.requests;
  server.requestTimeout = BUDGETS.bodyTimeoutMs;
  server.headersTimeout = BUDGETS.bodyTimeoutMs;
  server.timeout = BUDGETS.bodyTimeoutMs;
  server.maxRequestsPerSocket = 1000;
  await new Promise((ok, fail) => { server.once('error', fail); server.listen(port, host, ok); });
  return { server, handler, url: `http://${host === '::1' ? '[::1]' : host}:${server.address().port}` };
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const options = {};
  for (let i = 2; i < process.argv.length; i += 2) {
    const key = process.argv[i], value = process.argv[i + 1];
    if (!value || !['--host', '--port', '--state-file', '--static-dir'].includes(key)) throw new Error('usage: local.mjs [--host loopback] [--port 8787] [--state-file path] [--static-dir viewer-dist]');
    options[key.slice(2)] = value;
  }
  const port = options.port === undefined ? 8787 : Number(options.port);
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error('invalid port');
  const result = await createLocalServer({ host: options.host, port, storage: new LocalStorage(options['state-file']), staticDir: options['static-dir'] });
  console.log(`BMUX live relay listening at ${result.url}`); // No secrets, IDs or content.
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => {
    result.server.close(); result.server.closeAllConnections();
  });
}
