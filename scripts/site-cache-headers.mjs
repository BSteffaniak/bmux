// Pages response policy belongs to the immutable site artifact, not zone-wide rules.
import { createHash } from 'node:crypto';
import { readdir, readFile, writeFile } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import { pathToFileURL } from 'node:url';

const viewerFiles = ['canvas-viewer.js', 'live-viewer.js', 'live-page.js',
  'bmux_web_renderer.js', 'bmux_web_renderer_bg.wasm'];
const policy = '  Cache-Control: public, max-age=31536000, immutable\n';
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');

export async function generateHeaders(site) {
  const paths = [];
  const entries = async directory => {
    try { return await readdir(join(site, directory), { withFileTypes: true }); }
    catch (error) { if (error.code === 'ENOENT') return []; throw error; }
  };
  for (const directory of await entries('live-assets')) {
    if (!directory.isDirectory() || !/^[a-f0-9]{64}$/.test(directory.name)) continue;
    const prefix = `live-assets/${directory.name}`;
    // Verify the same ordered content identity used by build-web-renderer.sh.
    // Never make mutable/unverified files immutable merely because of their name.
    const digests = await Promise.all(viewerFiles.map(async name =>
      sha256(await readFile(join(site, prefix, name)))));
    if (sha256(digests.map(hash => hash + '\n').join('')) !== directory.name) {
      throw new Error('Viewer bundle identity mismatch');
    }
    paths.push(...viewerFiles.map(name => `/${prefix}/${name}`));
  }
  for (const file of await entries('js')) {
    // HyperChad generates this content-hashed runtime name (including plugins).
    if (file.isFile() && /^hyperchad-[a-f0-9]{10}(?:\.min)?\.js$/.test(file.name)) {
      paths.push(`/js/${file.name}`);
    }
  }
  paths.sort();
  // Pages supports at most 100 header rules. Fail rather than silently drop any.
  if (paths.length > 100) throw new Error('Pages header rule limit exceeded');
  await writeFile(join(site, '_headers'),
    '# Generated: only content-addressed assets; HTML and mutable paths retain Pages defaults.\n' +
    paths.map(path => `${path}\n${policy}\n`).join(''), { flag: 'wx' });
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  if (process.argv.length !== 3) throw new Error('Usage: node scripts/site-cache-headers.mjs SITE');
  await generateHeaders(resolve(process.argv[2]));
}
