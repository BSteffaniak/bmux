// Exercise the real bootstrap dispatch without starting either renderer.
// Run: node packages/web-renderer/tests/delivery-defaults.js
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(`${__dirname}/../browser/bootstrap.js`, 'utf8');
const dispatch = source.slice(source.indexOf('\nconst delivery ='));
async function check(search, defaults, expected, error = false) {
  const calls = [];
  const status = { textContent: '' };
  const context = vm.createContext({
    URLSearchParams, location: { search }, status, stopped: false,
    run: () => { calls.push(['frame']); return Promise.resolve(); },
    runDocument: adapter => { calls.push(['document', adapter]); return Promise.resolve(); },
    ...(defaults ? { bmuxViewerDefaults: defaults } : {}),
  });
  vm.runInContext(dispatch, context);
  await Promise.resolve();
  assert.deepEqual(calls, expected);
  assert.equal(status.textContent !== '', error);
}
(async () => {
  const defaults = { delivery: 'document', adapter: '/application-adapter.js' };
  await check('', undefined, [['frame']]);
  await check('', defaults, [['document', '/application-adapter.js']]);
  await check('?inspect=1', defaults, [['document', '/application-adapter.js']]);
  await check('?delivery=frame', defaults, [['frame']]);
  await check('?delivery=document&adapter=/other.js', defaults, [['document', '/other.js']]);
  await check('?delivery=document', defaults, [['document', '/application-adapter.js']]);
  await check('?delivery=document&adapter=/other.js', undefined, [['document', '/other.js']]);
  await check('?delivery=unsupported', defaults, [], true);
  await check('?delivery=', defaults, [], true);
  console.log('browser delivery defaults: PASS');
})().catch(error => { console.error(error); process.exitCode = 1; });
