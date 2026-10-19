import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const root = resolve(import.meta.dirname, '..');
const names = ['Production', 'production-approval'];
function fixture() {
  const response = {};
  for (const name of names) {
    const path = `repos/BSteffaniak/bmux/environments/${name}`;
    response[path] = { id: 1, name, can_admins_bypass: false,
      deployment_branch_policy: { protected_branches: false, custom_branch_policies: true },
      protection_rules: name === 'Production' ? [] : [{ type: 'required_reviewers',
        reviewers: [{ type: 'User', reviewer: { login: 'owner', id: 7 } }] }] };
    response[`${path}/deployment_protection_rules?per_page=100`] = {
      total_count: 0, custom_deployment_protection_rules: [] };
    response[`${path}/deployment-branch-policies?per_page=100`] = {
      total_count: 1, branch_policies: [{ id: 123, name: 'master', type: 'branch' }] };
  }
  for (const name of ['LIVE_FLY_APP', 'LIVE_FLY_ORG', 'LIVE_FLY_REGION'])
    response[`repos/BSteffaniak/bmux/actions/variables/${name}`] = { value: 'current' };
  return response;
}
function discover(t, responses, repo = 'BSteffaniak/bmux') {
  const dir = mkdtempSync(join(tmpdir(), 'bmux-github-migration-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  writeFileSync(join(dir, 'fixture.json'), JSON.stringify(responses));
  writeFileSync(join(dir, 'gh'), `#!${process.execPath}\nconst fs = require('node:fs');
if (process.argv.length !== 4 || process.argv[2] !== 'api') process.exit(2);
const responses = JSON.parse(fs.readFileSync(process.env.FIXTURE));
if (!(process.argv[3] in responses)) process.exit(3);
console.log(JSON.stringify(responses[process.argv[3]]));\n`, { mode: 0o700 });
  const output = join(dir, 'adoption.json');
  const result = spawnSync(process.execPath, ['scripts/migrate-live-github.mjs', output], {
    cwd: root, encoding: 'utf8', env: { ...process.env, GITHUB_REPOSITORY: repo,
      FIXTURE: join(dir, 'fixture.json'), PATH: `${dir}:${process.env.PATH}` },
  });
  return { result, output };
}
test('discovery preserves exact existing identities and values without API writes', t => {
  const { result, output } = discover(t, fixture());
  assert.equal(result.status, 0, result.stderr);
  const config = JSON.parse(readFileSync(output));
  assert.equal(config.approval_reviewer, 'owner');
  assert.deepEqual(config.existing_environments, names);
  assert.equal(config.existing_master_policy_ids.Production, 123);
  assert.equal(config.fly_config.app, 'current');
  assert.equal(JSON.parse(readFileSync(output + '.snapshot.json')).Production.id, 1);
});
test('discovery rejects unexpected state rather than changing it', t => {
  const path = 'repos/BSteffaniak/bmux/environments/Production';
  for (const change of [
    data => { data[path].name = 'production'; },
    data => { data[path].protection_rules = [{ type: 'unknown' }]; },
    data => { data[path].protection_rules = [{ type: 'wait_timer', wait_timer: 10 }]; },
    data => { data[path + '/deployment-branch-policies?per_page=100'].total_count = 2; },
    data => { data[path + '/deployment-branch-policies?per_page=100'].branch_policies[0].name = '*'; },
    data => { data[path + '/deployment_protection_rules?per_page=100'].total_count = 1; },
    data => { data[path].protection_rules = [{ type: 'required_reviewers', reviewers: [{ type: 'User' }] }]; },
  ]) {
    const data = fixture();
    change(data);
    assert.notEqual(discover(t, data).result.status, 0);
  }
  assert.notEqual(discover(t, fixture(), 'other/repo').result.status, 0);
});
function plan() {
  return { resource_changes: [
    ...names.map(name => ({ address: `github_repository_environment.release["${name}"]`, mode: 'managed',
      change: { actions: ['update'], importing: { id: `bmux:${name}` } } })),
    ...names.map(name => ({ address: `github_repository_environment_deployment_policy.master["${name}"]`, mode: 'managed',
      change: { actions: ['no-op'] } })),
    ...['LIVE_FLY_APP', 'LIVE_FLY_ORG', 'LIVE_FLY_REGION'].map(name => ({
      address: `github_actions_variable.fly["${name}"]`, mode: 'managed', change: { actions: ['no-op'] } })),
  ] };
}
function check(value) {
  return spawnSync(process.execPath, ['scripts/check-live-github-migration.mjs'], {
    cwd: root, encoding: 'utf8', input: JSON.stringify(value),
  });
}
test('plan guard allows imports and environment-only updates', () => {
  assert.equal(check(plan()).status, 0);
  const value = plan();
  value.resource_changes.forEach(r => { r.change.actions = ['no-op']; });
  assert.equal(check(value).status, 0);
});
test('plan guard rejects creates, deletes, replacement, variables drift and incomplete plans', () => {
  for (const actions of [['create'], ['delete'], ['delete', 'create']]) {
    const value = plan();
    value.resource_changes[0].change.actions = actions;
    assert.notEqual(check(value).status, 0);
  }
  const value = plan();
  value.resource_changes.at(-1).change.actions = ['update'];
  assert.notEqual(check(value).status, 0);
  value.resource_changes.pop();
  assert.notEqual(check(value).status, 0);
  assert.notEqual(check({ ...plan(), deferred_changes: [{}] }).status, 0);
});
test('initializer passes a JSON-suffixed backend and cleans up on success and failure', t => {
  const dir = mkdtempSync(join(tmpdir(), 'bmux-github-init-test-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  // Capture the backend while it exists, without invoking a provider or R2.
  writeFileSync(join(dir, 'tofu'), `#!${process.execPath}
const fs = require('node:fs');
const args = process.argv.slice(2);
const backend = args.find(arg => arg.startsWith('-backend-config=')).slice('-backend-config='.length);
fs.writeFileSync(process.env.CAPTURE, JSON.stringify({ args, backend,
  config: JSON.parse(fs.readFileSync(backend, 'utf8')),
  mode: fs.statSync(backend).mode & 0o777 }));
process.exit(Number(process.env.TOFU_EXIT));
`, { mode: 0o700 });
  for (const exit of [0, 1]) {
    const capture = join(dir, `capture-${exit}.json`);
    const result = spawnSync('bash', ['scripts/live-github-init.sh'], {
      cwd: root, encoding: 'utf8', env: { ...process.env,
        PATH: `${dir}:${process.env.PATH}`, TMPDIR: dir, CAPTURE: capture, TOFU_EXIT: String(exit),
        CLOUDFLARE_ACCOUNT_ID: 'a'.repeat(32), LIVE_STATE_BUCKET: 'offline-test-state',
        R2_STATE_ACCESS_KEY_ID: 'offline-key', R2_STATE_SECRET_ACCESS_KEY: 'offline-secret',
        TOFU_STATE_ENCRYPTION_PASSPHRASE: 'offline-test-only-passphrase',
      },
    });
    assert.equal(result.status, exit, result.stderr);
    const { args, backend, config, mode } = JSON.parse(readFileSync(capture));
    assert.ok(backend.endsWith('.tfbackend.json'));
    assert.ok(args.includes('init'));
    assert.ok(args.includes('-lockfile=readonly'));
    assert.equal(config.key, 'bmux/github/production/terraform.tfstate');
    assert.equal(config.endpoint, `https://${'a'.repeat(32)}.r2.cloudflarestorage.com`);
    assert.equal(config.bucket, 'offline-test-state');
    assert.equal(config.use_lockfile, true);
    assert.equal(mode, 0o600);
    assert.ok(!existsSync(backend));
    assert.ok(!existsSync(join(backend, '..')));
    assert.ok(!JSON.stringify(config).includes('offline-secret'));
  }
});

test('workflow keeps one gate, encrypted durable state and exact same-run plan binding', () => {
  const workflow = readFileSync(join(root, '.github/workflows/migrate-live-github.yml'), 'utf8');
  assert.equal((workflow.match(/environment: production-approval/g) ?? []).length, 1);
  for (const text of ['workflow_dispatch:', 'needs: [plan, approve]', 'cancel-in-progress: false',
    'github.run_attempt', 'APPROVED_DIGEST', 'sha256sum -c SHA256SUMS', 'cmp ',
    'source scripts/live-github-init.sh', '-detailed-exitcode', 'retention-days: 1']) assert.ok(workflow.includes(text), text);
  assert.ok(!workflow.includes('tofu destroy'));
  assert.ok(!workflow.includes('-auto-approve'));
  const backend = readFileSync(join(root, 'scripts/live-github-init.sh'), 'utf8');
  assert.ok(backend.includes('bmux/github/production/terraform.tfstate'));
  assert.ok(!backend.includes('bmux/live/production/terraform.tfstate'));
  assert.ok(backend.includes('use_lockfile: true'));
  const encryption = readFileSync(join(root, 'infra/live-github/backend.tf'), 'utf8');
  assert.equal((encryption.match(/enforced = true/g) ?? []).length, 2);
});
