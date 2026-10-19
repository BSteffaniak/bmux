#!/usr/bin/env node
// Emit only public resource/action metadata, never provider state or credentials.
import { readFileSync } from 'node:fs';
const plan = JSON.parse(readFileSync(0, 'utf8'));
const environments = new Set(['github_repository_environment.release["Production"]',
  'github_repository_environment.release["production-approval"]']);
const expected = new Set([...environments,
  ...['Production', 'production-approval'].map(name =>
    `github_repository_environment_deployment_policy.master["${name}"]`),
  ...['LIVE_FLY_APP', 'LIVE_FLY_ORG', 'LIVE_FLY_REGION'].map(name =>
    `github_actions_variable.fly["${name}"]`)]);
const seen = new Set();
for (const resource of plan.resource_changes ?? []) {
  if (resource.mode === 'data') continue;
  const actions = resource.change.actions;
  if (!expected.has(resource.address) || seen.has(resource.address) ||
      !(actions.length === 1 && (actions[0] === 'no-op' ||
        (actions[0] === 'update' && environments.has(resource.address)))))
    throw new Error('Migration only permits adoption and environment policy updates; inspect manually');
  seen.add(resource.address);
  console.log(`${resource.address}: ${actions.join(',')}${resource.change.importing ? ' (import)' : ''}`);
}
if (seen.size !== expected.size) throw new Error('Incomplete migration plan');
if (plan.errored || plan.deferred_changes?.length) throw new Error('Incomplete Terraform plan');
