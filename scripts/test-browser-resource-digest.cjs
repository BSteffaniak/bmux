#!/usr/bin/env node
// Verify the production HTTP-LAN fallback against independent SHA-256 vectors.
const assert = require('node:assert/strict');
const { createHash, webcrypto } = require('node:crypto');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('packages/web-renderer/browser/bootstrap.js', 'utf8');
const helper = source.slice(source.indexOf('async function resourceDigest'), source.indexOf('function documentAdapterPath'));
async function run() {
  for (const crypto of [undefined, {}, webcrypto]) {
    const context = vm.createContext({ crypto, stopped: false, setTimeout });
    vm.runInContext(helper, context);
    for (const length of [0, 1, 3, 55, 56, 63, 64, 65, 127, 128, 1000, 65536, 1000000, 8 * 1024 * 1024]) {
      context.bytes = Uint8Array.from({ length }, (_, i) => (i * 37) & 255);
      const expected = createHash('sha256').update(context.bytes).digest('hex');
      const actual = await vm.runInContext('resourceDigest(bytes)', context);
      assert.equal(Buffer.from(actual).toString('hex'), expected, `length ${length}`);
    }
  }
  const unsupported = vm.createContext({ crypto: {}, DataView: undefined });
  vm.runInContext(helper, unsupported);
  await assert.rejects(vm.runInContext('resourceDigest([])', unsupported), /Browser cannot verify document resources/);
  console.log('resource SHA-256 native and HTTP-LAN fallback: PASS');
}
run().catch(error => { console.error(error); process.exitCode = 1; });
