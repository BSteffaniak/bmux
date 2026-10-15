#!/usr/bin/env node
// Exercise the production delivery helpers without a DOM or WASM renderer.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('packages/web-renderer/browser/bootstrap.js', 'utf8');
const helpers = source.slice(source.indexOf('let diagnosticSequence'), source.indexOf('async function boundedBytes'));
let response = { status: 200 };
let failure = false;
const context = vm.createContext({
  window: {}, performance: { now: () => 100 }, console: { info: () => {} },
  token: 'SECRET_CAPABILITY', authorized: value => value,
  fetch: async () => { if (failure) throw new Error('SECRET_RESOURCE_URL'); return response; },
});
vm.runInContext(helpers, context);
async function run() {
  await vm.runInContext("request('/document?after=SECRET_CURSOR', {signal: {aborted: false}})", context);
  assert.equal(context.window.__bmuxDiagnostics().events.length, 0);
  failure = true;
  await assert.rejects(vm.runInContext("request('/resource/SECRET_ID', {signal: {aborted: false}})", context), /HTTP host unreachable/);
  await assert.rejects(vm.runInContext("request('/document', {signal: {aborted: true}})", context), /deadline exceeded/);
  failure = false;
  response = { status: 503 };
  await vm.runInContext("request('/document', {signal: {aborted: false}})", context);
  const evidence = context.window.__bmuxDiagnostics();
  assert.deepEqual(Array.from(evidence.events, event => event.code), ['network_unavailable', 'request_aborted', 'http_rejected']);
  assert.equal(evidence.events[2].status, 503);
  assert(!JSON.stringify(evidence).includes('SECRET'));
  vm.runInContext("for (let i = 0; i < 100; i++) diagnostic('document_repair', {failures: i}); diagnostic('recovered', {failures: 100});", context);
  const bounded = context.window.__bmuxDiagnostics();
  assert.equal(bounded.events.length, 64);
  assert.equal(bounded.events.at(-1).code, 'recovered');
  bounded.events[0].code = 'modified';
  assert.notEqual(context.window.__bmuxDiagnostics().events[0].code, 'modified');
  console.log('browser delivery diagnostics: PASS');
}
run().catch(error => { console.error(error); process.exitCode = 1; });
