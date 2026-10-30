#!/usr/bin/env python3
"""Incident-scoped Fly ownership repair; never changes infrastructure or prints state."""
import argparse
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys

SPEC = importlib.util.spec_from_file_location('inspection', Path(__file__).with_name('inspect-live-mounts.py'))
inspection = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inspection)

# Reviewed evidence: read-only run 41094527560. Never infer new incident scope.
APP = 'bmux-live'
MACHINE = '8d99733c27d018'
PREVIOUS = 'vol_vxmzq7dl9yw1x1z4'
LIVE = 'vol_vjy7qdp90wqoy7ov'
PROVIDER = 'provider["registry.terraform.io/ampbase-io/fly"]'


class ReconciliationError(ValueError):
    pass


def require(condition):
    if not condition:
        raise ReconciliationError('Ownership precondition failed; no repair authorized, values withheld')


def attributes(state, kind, name):
    rows = [r for r in state['resources'] if r.get('mode') == 'managed'
            and r.get('type') == kind and r.get('name') == name and not r.get('module')]
    require(len(rows) == 1 and rows[0].get('provider') == PROVIDER)
    instances = rows[0].get('instances', [])
    require(len(instances) == 1 and not any(k in instances[0] for k in ('index_key', 'deposed')))
    require(instances[0].get('schema_version') == (1 if kind == 'fly_machine' else 0))
    return instances[0]['attributes']


def prepare(state, app, get=inspection.api_get, token=''):
    require(app == APP and state.get('version') == 4)
    require(state.get('terraform_version') == '1.10.9')
    require(type(state.get('serial')) is int and state['serial'] >= 0)
    require(isinstance(state.get('lineage'), str) and bool(state['lineage']))
    candidate = copy.deepcopy(state)
    machine = attributes(candidate, 'fly_machine', 'live')
    volume = attributes(candidate, 'fly_volume', 'authority')
    require(machine.get('id') == MACHINE and machine.get('app') == APP and volume.get('app') == APP)
    require(machine.get('region') == 'iad' and volume.get('region') == 'iad')
    require(volume.get('name') == 'live_authority' and volume.get('encrypted') is True)
    require(volume.get('size_gb') == 1)
    mounts = machine.get('mount', [])
    require(len(mounts) == 1 and mounts[0].get('path') == '/data')
    require(volume.get('id') in (PREVIOUS, LIVE) and mounts[0].get('volume') in (PREVIOUS, LIVE))
    require(volume.get('attached_machine_id') in (None, '', MACHINE))
    for flag in (volume.get('readopt_on_host_migration'), mounts[0].get('readopt_on_host_migration')):
        require(flag is None or type(flag) is bool)

    live_machine = get('apps/' + APP + '/machines/' + MACHINE, token)
    require(live_machine.get('id') == MACHINE and live_machine.get('region') == 'iad')
    live_mounts = live_machine.get('config', {}).get('mounts', [])
    require(len(live_mounts) == 1 and live_mounts[0].get('path') == '/data'
            and live_mounts[0].get('volume') == LIVE)
    inventory = get('apps/' + APP + '/volumes', token)
    require(isinstance(inventory, list))
    require(not any(v.get('id') == PREVIOUS for v in inventory))
    live_volumes = [v for v in inventory if v.get('id') == LIVE]
    require(len(live_volumes) == 1)
    live_volume = live_volumes[0]
    require(live_volume.get('attached_machine_id') == MACHINE and live_volume.get('name') == 'live_authority')
    require(live_volume.get('region') == 'iad' and live_volume.get('encrypted') is True)
    require(live_volume.get('size_gb') == 1)
    # Advance by exactly one, never force: an intervening write advances the
    # remote serial to this value or beyond. OpenTofu rejects equal-serial
    # different content as well as newer remote serials and other lineages.
    candidate['serial'] = state['serial'] + 1
    # Only these observed ownership/opt-in fields may otherwise change.
    volume['id'] = LIVE
    volume['attached_machine_id'] = MACHINE
    volume['readopt_on_host_migration'] = True
    mounts[0]['volume'] = LIVE
    mounts[0]['readopt_on_host_migration'] = True
    return candidate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--verify', action='store_true')
    parser.add_argument('source', type=Path)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        require(args.source.stat().st_size <= 32 * 1024 * 1024)
        original = json.loads(args.source.read_text())
        if args.verify:
            require(args.destination.stat().st_size <= 32 * 1024 * 1024)
            actual = json.loads(args.destination.read_text())
            require(type(actual.get('serial')) is int and actual['serial'] >= original['serial'])
            # OpenTofu alone may advance serial. All other canonical data must
            # survive unchanged, not just the two repaired resource projections.
            comparable = copy.deepcopy(actual)
            comparable['serial'] = original['serial']
            require(comparable == original)
            refreshed = prepare(actual, os.environ['LIVE_FLY_APP'], token=os.environ['FLY_API_TOKEN'])
            refreshed['serial'] = actual['serial']
            require(refreshed == actual)
            print('Canonical ownership verified after state-only reconciliation.')
            return 0
        candidate = prepare(original, os.environ['LIVE_FLY_APP'], token=os.environ['FLY_API_TOKEN'])
        with args.destination.open('x') as stream:
            json.dump(candidate, stream)
        print('Incident-scoped ownership candidate verified; no infrastructure mutation performed.')
    except (ValueError, OSError, KeyError, TypeError, AttributeError, RecursionError):
        print('Ownership reconciliation blocked; input/API values withheld.', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
