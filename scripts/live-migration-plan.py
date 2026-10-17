#!/usr/bin/env python3
"""Bind migration approval to a candidate, encrypted state and private saved plan.

Only fixed resource addresses/actions and a digest may leave the private runner.
No plans, state values or credentials are included in the public summary.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re

SPEC = importlib.util.spec_from_file_location('live_plan_policy', Path(__file__).with_name('live-plan-policy.py'))
POLICY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(POLICY)
CONFIG = ('CLOUDFLARE_ACCOUNT_ID', 'CLOUDFLARE_ZONE_ID', 'LIVE_HOSTNAME',
          'LIVE_PAGES_PROJECT', 'LIVE_PUBLISHER_KEY_SHA256', 'LIVE_METRICS_KEY_SHA256',
          'LIVE_FLY_APP', 'LIVE_FLY_ORG', 'LIVE_FLY_REGION', 'LIVE_RELAY_HOSTNAME',
          'LIVE_STATE_BUCKET', 'LIVE_ENVIRONMENT', 'LIVE_STATE_REGION')


def read(path):
    if path.stat().st_size > 32 * 1024 * 1024:
        raise ValueError('Oversized private migration input')
    return json.loads(path.read_bytes())


def changes(plan):
    if (not isinstance(plan, dict) or plan.get('errored')
            or plan.get('complete') is False or plan.get('deferred_changes')):
        raise ValueError('Incomplete migration plan')
    resources = plan.get('resource_changes', [])
    if not isinstance(resources, list):
        raise ValueError('Invalid migration changes')
    seen = set()
    for item in resources:
        address = item.get('address')
        if address in seen or address not in POLICY.CONFIGURE_RESOURCES | POLICY.WORKER_RESOURCES:
            raise ValueError('Unexpected migration resource')
        seen.add(address)
        change = item['change']
        if (change.get('actions') not in (['no-op'], ['read'])
                and address not in POLICY.WORKER_RESOURCES
                and not address.startswith('fly_')
                and address not in {'cloudflare_dns_record.relay', 'cloudflare_dns_record.relay_acme'}):
            raise ValueError('Migration must preserve Pages, website DNS and state resources')
        if change.get('importing') or item.get('deposed'):
            raise ValueError('Migration cannot import or replace resources')
        POLICY.validate({'resource_changes': [item]}, configure=True,
                        retire_worker=address in POLICY.WORKER_RESOURCES)
    if plan.get('applyable') is False:
        raise ValueError('Migration plan is not applyable')
    return resources


def digest(plan, state, artifact, env):
    resources = sorted(changes(plan), key=lambda item: item['address'])
    # This flow is intentionally an existing-Worker cutover, not bootstrap or
    # adoption. A separately managed Fly installation needs explicit review.
    if not any(r['address'] == 'cloudflare_worker.live' and r['change']['actions'] == ['delete'] for r in resources):
        raise ValueError('No existing managed Worker to migrate')
    for address in ('fly_app.live', 'fly_machine.live', 'fly_volume.authority'):
        if not any(r['address'] == address and r['change']['actions'] == ['create'] for r in resources):
            raise ValueError('Existing or partially provisioned Fly resources require explicit recovery')
    revision = (artifact / 'REVISION').read_text().strip()
    source = state.get('sha256')
    if not re.fullmatch(r'[a-f0-9]{40,64}', revision) or not isinstance(source, str) or not re.fullmatch(r'[a-f0-9]{64}', source):
        raise ValueError('Missing verified candidate or encrypted state identity')
    value = dict(schema=1, revision=revision,
                 inventory_sha256=hashlib.sha256((artifact / 'SHA256SUMS').read_bytes()).hexdigest(),
                 state_sha256=source, configuration={k: env.get(k, '') for k in CONFIG},
                 changes=resources, outputs=plan.get('output_changes', {}))
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def compatible(expected, actual, unknown=False):
    if unknown is True:
        return True
    if isinstance(unknown, dict) and unknown and expected is None:
        expected = {}
    if isinstance(unknown, list) and expected is None:
        expected = [None] * len(unknown)
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(
            compatible(expected.get(k), actual.get(k), unknown.get(k, False) if isinstance(unknown, dict) else False)
            for k in set(expected) | (set(unknown) if isinstance(unknown, dict) else set())) and all(k in expected or (isinstance(unknown, dict) and k in unknown) for k in actual)
    if isinstance(expected, list):
        return isinstance(actual, list) and len(expected) == len(actual) and all(
            compatible(v, actual[i], unknown[i] if isinstance(unknown, list) and i < len(unknown) else False)
            for i, v in enumerate(expected))
    return expected == actual


def validate_phase(approved, phase, stage):
    expected = {r['address']: r['change'] for r in changes(approved)}
    required = {address for address, change in expected.items()
                if change['actions'] not in (['no-op'], ['read'])
                and ((stage == 'retire' and address in POLICY.WORKER_RESOURCES)
                     or (stage == 'app' and address == 'fly_app.live')
                     or (stage == 'resources' and address not in POLICY.WORKER_RESOURCES | {'fly_app.live'}))}
    actual_mutations = {item['address'] for item in changes(phase)
                        if item['change']['actions'] not in (['no-op'], ['read'])}
    if actual_mutations != required:
        raise ValueError('Missing or unexpected approved migration phase actions')
    for item in changes(phase):
        change = item['change']
        original = expected.get(item['address'])
        if change['actions'] in (['no-op'], ['read']):
            if original is None:
                raise ValueError('Unexpected migration phase state')
            if original['actions'] == ['delete']:
                raise ValueError('Approved retired resource still present')
            if not compatible(original.get('after'), change.get('after'), original.get('after_unknown', False)):
                raise ValueError('Completed migration resource differs from approval')
            continue
        if original is None or change['actions'] != original['actions']:
            raise ValueError('Migration phase differs from approved resource actions')
        if original.get('before') != change.get('before'):
            raise ValueError('Migration phase preconditions changed')
        if not compatible(original.get('after'), change.get('after'), original.get('after_unknown', False)):
            raise ValueError('Migration phase configuration differs from approval')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('plan', type=Path)
    parser.add_argument('--state', type=Path)
    parser.add_argument('--artifact', type=Path)
    parser.add_argument('--expected')
    parser.add_argument('--approved-plan', type=Path)
    parser.add_argument('--stage', choices=('retire', 'app', 'resources'))
    args = parser.parse_args()
    try:
        plan = read(args.plan)
        if args.approved_plan:
            if not args.stage:
                raise ValueError('Missing phase identity')
            validate_phase(read(args.approved_plan), plan, args.stage)
            return
        if not args.state or not args.artifact:
            raise ValueError('Missing migration identity')
        value = digest(plan, read(args.state), args.artifact, os.environ)
        if args.expected:
            if not re.fullmatch(r'[a-f0-9]{64}', args.expected) or args.expected != value:
                raise ValueError('Migration approval is stale')
            return
        lines = ['## Reviewed Worker → Fly migration', '', 'Downtime and paid Fly resources require approval.',
                 'Candidate: `' + (args.artifact / 'REVISION').read_text().strip() + '`', '']
        for resource in changes(plan):
            actions = resource['change']['actions']
            if actions not in (['no-op'], ['read']):
                line = '- `' + resource['address'] + '`: ' + ', '.join(actions)
                # Worker identifiers are public provider names, never arbitrary
                # provider output. Do not print fields outside this allowlist.
                if resource['address'] in POLICY.WORKER_RESOURCES:
                    before = resource['change'].get('before') or {}
                    identity = before.get('name', before.get('id'))
                    if isinstance(identity, str) and re.fullmatch(r'[A-Za-z0-9_.:/-]{1,160}', identity):
                        line += ' (existing identity: `' + identity + '`)'
                lines.append(line)
        for name in ('LIVE_FLY_APP', 'LIVE_FLY_ORG', 'LIVE_FLY_REGION'):
            public_value = os.environ.get(name, '')
            if public_value and re.fullmatch(r'[a-z0-9][a-z0-9-]{0,62}', public_value):
                lines.append('- ' + name + ': `' + public_value + '`')
        lines += ['', 'Approval digest: `' + value + '`',
                  'If a phase fails, stop and reconcile canonical state and the migration fence; do not rerun blindly.']
        print('\n'.join(lines))
        if os.environ.get('GITHUB_STEP_SUMMARY'):
            with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as stream:
                stream.write('\n'.join(lines) + '\n')
        if os.environ.get('GITHUB_OUTPUT'):
            with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
                stream.write('migration_digest=' + value + '\n')
    except (OSError, ValueError, TypeError, KeyError, AttributeError):
        parser.exit(2, 'Migration plan rejected; candidate/state/configuration changed or explicit recovery required. Private values suppressed.\n')


if __name__ == '__main__':
    main()
