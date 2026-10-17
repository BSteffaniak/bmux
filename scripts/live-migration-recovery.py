#!/usr/bin/env python3
"""Explicit recovery for the inspected Worker-retired, managed-Fly-app cutover."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), ROOT / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


STATE = load('archive-live-state')
PLAN = load('live-migration-plan')


def validate(plan):
    checked = dict(plan)
    # Recovery after a health-check failure may have no infrastructure changes.
    # Still validate every resource and recheck the applying marker.
    checked.pop('applyable', None)
    resources = PLAN.changes(checked)
    indexed = {r['address']: r['change'] for r in resources}
    if any(address in indexed for address in PLAN.POLICY.WORKER_RESOURCES):
        raise ValueError('Recovery requires Worker retirement already recorded')
    if indexed.get('fly_app.live', {}).get('actions') != ['no-op']:
        raise ValueError('Recovery must preserve the managed Fly app')
    allowed = {'fly_machine.live', 'fly_volume.authority', 'fly_ip.v4', 'fly_ip.v6',
               'fly_cert.live', 'fly_cert_validation.live',
               'cloudflare_dns_record.relay', 'cloudflare_dns_record.relay_acme'}
    for address, change in indexed.items():
        if change['actions'] not in (['no-op'], ['read']):
            if address not in allowed or change['actions'] != ['create']:
                raise ValueError('Recovery permits only missing relay resource creation')
    for address in ('fly_machine.live', 'fly_volume.authority'):
        if indexed.get(address, {}).get('actions') not in (['create'], ['no-op']):
            raise ValueError('Recovery requires the relay machine and authority volume')
    return resources


def snapshot(store):
    store.migration_authority()
    remote = store.migration_marker()
    if remote is None or remote[0]['phase'] != 'applying':
        raise ValueError('Recovery requires existing applying migration marker')
    current = store.migration_state()
    if store.migration_marker() != remote:
        raise ValueError('Migration marker changed')
    marker, etag = remote
    receipt = dict(schema=1, key=store.migration_key(), authority=store.migration_authority(),
                   etag=etag, operation=marker['operation'], migration_digest=marker['migration_digest'],
                   source_sha256=marker['source_sha256'])
    receipt['binding'] = STATE.migration_binding(receipt)
    return {'state': current, 'marker': marker, 'etag': etag, 'receipt': receipt}


def digest(plan, evidence, artifact, env):
    resources = sorted(validate(plan), key=lambda r: r['address'])
    value = {'schema': 1, 'state': evidence['state'], 'marker': evidence['marker'], 'etag': evidence['etag'],
             'revision': (artifact / 'REVISION').read_text().strip(),
             'inventory': hashlib.sha256((artifact / 'SHA256SUMS').read_bytes()).hexdigest(),
             'configuration': {key: env.get(key, '') for key in PLAN.CONFIG}, 'resources': resources,
             'outputs': plan.get('output_changes', {})}
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('snapshot', 'verify', 'plan', 'complete'))
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--plan', type=Path)
    parser.add_argument('--artifact', type=Path)
    parser.add_argument('--expected')
    args = parser.parse_args()
    if args.action == 'snapshot':
        store = STATE.Store()
        try:
            value = snapshot(store)
            with args.evidence.open('x') as stream:
                json.dump(value, stream)
        finally:
            store.temp.cleanup()
    elif args.action == 'verify':
        store = STATE.Store()
        try:
            expected = PLAN.read(args.evidence)
            current = snapshot(store)
            if current != expected:
                raise ValueError('Recovery state or marker changed before apply')
        finally:
            store.temp.cleanup()
    elif args.action == 'plan':
        plan = PLAN.read(args.plan)
        evidence = PLAN.read(args.evidence)
        value = digest(plan, evidence, args.artifact, os.environ)
        if args.expected:
            if args.expected != value:
                raise ValueError('Recovery approval changed; no apply allowed')
            return
        lines = ['## Explicit partial-cutover recovery',
                 'Existing Fly app preserved; no Worker retirement or imports.']
        for resource in validate(plan):
            if resource['change']['actions'] == ['create']:
                lines.append('- create `' + resource['address'] + '`')
        lines.append('Recovery approval digest: ' + value)
        text = '\n'.join(lines)
        print(text)
        if os.environ.get('GITHUB_STEP_SUMMARY'):
            with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as stream:
                stream.write(text + '\n')
        if os.environ.get('GITHUB_OUTPUT'):
            with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
                stream.write('recovery_digest=' + value + '\n')
    else:
        # Only the original applying marker identity may be completed. Its
        # conditional ETag write is performed by the existing fenced protocol.
        evidence = PLAN.read(args.evidence)
        receipt = args.evidence.with_name('recovery-receipt.json')
        with receipt.open('x') as stream:
            json.dump(evidence['receipt'], stream)
        store = STATE.Store()
        try:
            result = store.migration_complete(receipt)
            print(json.dumps({'status': result['status']}))
        finally:
            store.temp.cleanup()
            receipt.unlink(missing_ok=True)


if __name__ == '__main__':
    os.umask(0o077)
    try:
        main()
    except (OSError, ValueError, TypeError, KeyError, AttributeError, STATE.StateError):
        raise SystemExit('Recovery rejected; preserve canonical state and marker. Private values suppressed.') from None
