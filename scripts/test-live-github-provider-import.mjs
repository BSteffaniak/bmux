// Real pinned provider against a loopback GitHub fixture: no live credentials,
// remote state, apply, or cloud calls. Exercises import, unlike mock_provider.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, writeFile, readFile, copyFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawn } from 'node:child_process';

const root = resolve(import.meta.dirname, '..');
function run(args, cwd, env) {
  return new Promise((resolve, reject) => {
    const child = spawn('tofu', args, { cwd, env });
    let stdout = '', stderr = '';
    child.stdout.on('data', bytes => { stdout += bytes; });
    child.stderr.on('data', bytes => { stderr += bytes; });
    child.on('error', reject);
    child.on('close', status => resolve({ status, stdout, stderr }));
  });
}
test('real GitHub provider imports policy identity without proposing replacements', { timeout: 60000 }, async t => {
  const dir = await mkdtemp(join(tmpdir(), 'bmux-github-provider-import-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const unexpected = [];
  const requests = [];
  const server = createServer((req, res) => {
    requests.push(`${req.method} ${req.url}`);
    let value;
    const match = req.url.match(/^\/api\/v3\/repos\/owner\/repo\/environments\/(Production|production-approval)\/deployment-branch-policies\/(123|456)$/);
    if (req.method === 'GET' && match) {
      value = { id: Number(match[2]), name: 'master', type: 'branch' };
    } else if (req.method === 'GET' && req.url === '/api/v3/orgs/owner') {
      res.statusCode = 404; // Personal-account owner, not an organization.
      value = { message: 'Not Found' };
    } else if (req.method === 'GET' && req.url === '/api/v3/users/owner') {
      value = { login: 'owner', id: 7, type: 'User' };
    } else {
      unexpected.push(`${req.method} ${req.url}`);
      res.statusCode = 500;
      value = { message: 'Unexpected fixture request' };
    }
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(value));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const { port } = server.address();
  // Copy only the pinned provider requirement, not the production backend.
  const source = await readFile(join(root, 'infra/live-github/main.tf'), 'utf8');
  const version = source.match(/\n      version = "(= [0-9.]+)"/)[1];
  const config = `terraform {
  required_providers { github = { source = "integrations/github", version = "${version}" } }
}
provider "github" {
  owner = "owner"
  token = "offline-fixture-only"
  base_url = "http://127.0.0.1:${port}/api/v3/"
}
resource "github_repository_environment_deployment_policy" "master" {
  for_each = { Production = 123, production-approval = 456 }
  repository = "repo"
  environment = each.key
  branch_pattern = "master"
  lifecycle { prevent_destroy = true }
}
import {
  for_each = { Production = 123, production-approval = 456 }
  to = github_repository_environment_deployment_policy.master[each.key]
  id = "repo:\${each.key}:\${each.value}"
}
`;
  await writeFile(join(dir, 'main.tf'), config);
  await copyFile(join(root, 'infra/live-github/.terraform.lock.hcl'), join(dir, '.terraform.lock.hcl'));
  // Deliberately strip ambient credentials and Terraform CLI overrides.
  const env = { PATH: process.env.PATH, HOME: dir, TMPDIR: dir, TF_IN_AUTOMATION: '1' };
  const init = await run(['init', '-backend=false', '-input=false', '-lockfile=readonly',
    `-plugin-dir=${join(root, 'infra/live-github/.terraform/providers')}`], dir, env);
  assert.equal(init.status, 0, init.stdout + init.stderr);
  const plan = await run(['plan', '-input=false', '-out=import.tfplan'], dir, env);
  assert.equal(plan.status, 0, plan.stdout + plan.stderr);
  const shown = await run(['show', '-json', 'import.tfplan'], dir, env);
  assert.equal(shown.status, 0, shown.stderr);
  const resources = JSON.parse(shown.stdout).resource_changes;
  assert.equal(resources.length, 2);
  for (const resource of resources) {
    assert.deepEqual(resource.change.actions, ['no-op']);
    assert.equal(resource.change.after.repository, 'repo');
    assert.ok(['Production', 'production-approval'].includes(resource.change.after.environment));
    assert.ok(resource.change.importing);
  }
  assert.deepEqual(unexpected, []);
  assert.ok(requests.some(path => path.endsWith('/deployment-branch-policies/123')));
  assert.ok(requests.some(path => path.endsWith('/deployment-branch-policies/456')));
});
