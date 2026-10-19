#!/usr/bin/env node
// Temporary migration helper: read GitHub metadata, never mutate through its API.
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
const repo = process.env.GITHUB_REPOSITORY;
if (repo !== 'BSteffaniak/bmux') throw new Error('Canonical repository required');
function get(path) {
  return JSON.parse(execFileSync('gh', ['api', path], { maxBuffer: 1048576, timeout: 30000 }));
}
const names = ['Production', 'production-approval'];
const policies = {};
const snapshots = {};
let reviewer;
for (const name of names) {
  const endpoint = `repos/${repo}/environments/${name}`;
  const env = get(endpoint);
  const custom = get(`${endpoint}/deployment_protection_rules?per_page=100`);
  if (custom.total_count !== 0 || custom.custom_deployment_protection_rules?.length !== 0)
    throw new Error('Custom protections require explicit reconciliation');
  if (env.name !== name || env.protection_rules.some(rule =>
    !['wait_timer', 'required_reviewers', 'branch_policy'].includes(rule.type)))
    throw new Error('Unexpected environment protection');
  if (env.protection_rules.some(rule => rule.type === 'wait_timer' && rule.wait_timer !== 0))
    throw new Error('Existing wait timer requires explicit review');
  const branches = get(`${endpoint}/deployment-branch-policies?per_page=100`);
  if (branches.total_count !== 1 || branches.branch_policies.length !== 1 ||
      branches.branch_policies[0].name !== 'master' || branches.branch_policies[0].type !== 'branch')
    throw new Error('Expected exactly one master-only policy');
  policies[name] = branches.branch_policies[0].id;
  snapshots[name] = { id: env.id, protection_rules: env.protection_rules,
    can_admins_bypass: env.can_admins_bypass, deployment_branch_policy: env.deployment_branch_policy,
    branches: branches.branch_policies };
  const reviews = env.protection_rules.filter(rule => rule.type === 'required_reviewers');
  if (name === 'Production' && reviews.some(rule => rule.reviewers.length))
    throw new Error('Production has an unexpected review gate; reconcile explicitly');
  if (name === 'production-approval') {
    const users = reviews.flatMap(rule => rule.reviewers);
    if (users.length !== 1 || users[0].type !== 'User')
      throw new Error('Expected one existing approval reviewer');
    reviewer = users[0].reviewer.login;
  }
}
const value = name => get(`repos/${repo}/actions/variables/${name}`).value;
const config = {
  owner: 'BSteffaniak', repository: 'bmux', approval_reviewer: reviewer,
  fly_config: { app: value('LIVE_FLY_APP'), org: value('LIVE_FLY_ORG'), region: value('LIVE_FLY_REGION') },
  existing_environments: names, existing_master_policy_ids: policies,
  existing_variables: ['LIVE_FLY_APP', 'LIVE_FLY_ORG', 'LIVE_FLY_REGION'],
};
writeFileSync(process.argv[2], JSON.stringify(config), { mode: 0o600, flag: 'wx' });
writeFileSync(process.argv[2] + '.snapshot.json', JSON.stringify(snapshots), { mode: 0o600, flag: 'wx' });
