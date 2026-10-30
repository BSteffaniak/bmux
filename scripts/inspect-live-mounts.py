#!/usr/bin/env python3
"""Read-only ownership inspection. Never print raw state, plans, API bodies or credentials."""
import argparse
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.request


class InspectionError(ValueError):
    pass


def identity(value, kind):
    patterns = {'machine': r'[a-f0-9]{14}', 'volume': r'vol_[a-z0-9]{14,32}',
                'app': r'[a-z0-9][a-z0-9-]{0,62}'}
    if not isinstance(value, str) or not re.fullmatch(patterns[kind], value):
        raise InspectionError('Missing or invalid ' + kind + ' identity; raw value withheld')
    return value


def resources(values):
    root = values.get('root_module', {})
    # These are deliberately root-owned resources, not same-named child modules.
    return {r['address']: r.get('values', {}) for r in root.get('resources', [])
            if r.get('address') in ('fly_machine.live', 'fly_volume.authority')}


def selected(values, app):
    rows = resources(values)
    machine = rows.get('fly_machine.live', {})
    volume = rows.get('fly_volume.authority', {})
    if machine.get('app') != app or volume.get('app') != app:
        raise InspectionError('State/plan app does not match the explicitly selected Fly app')
    mounts = machine.get('mount')
    if not isinstance(mounts, list) or len(mounts) != 1 or mounts[0].get('path') != '/data':
        raise InspectionError('Expected exactly one authority mount at /data')
    return {'machine_id': identity(machine.get('id'), 'machine'),
            'volume_resource_id': identity(volume.get('id'), 'volume'),
            'mounted_volume_id': identity(mounts[0].get('volume'), 'volume'),
            'volume_resource_attached_machine_id': (identity(volume['attached_machine_id'], 'machine')
                 if volume.get('attached_machine_id') else None),
            'volume_reconciliation_enabled': volume.get('readopt_on_host_migration') is True,
            'mount_reconciliation_enabled': mounts[0].get('readopt_on_host_migration') is True}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def api_get(path, token):
    request = urllib.request.Request('https://api.machines.dev/v1/' + path,
                                     headers={'Authorization': 'Bearer ' + token})
    try:
        with urllib.request.build_opener(NoRedirect).open(request, timeout=20) as response:
            data = response.read(1048577)
        if len(data) > 1048576:
            raise InspectionError('Fly inspection response too large; body withheld')
        return json.loads(data)
    except urllib.error.HTTPError as error:
        code = error.code
        error.close()
        raise InspectionError('Fly read-only inspection HTTP ' + str(code) + '; body withheld') from None
    except (OSError, ValueError):
        raise InspectionError('Fly read-only inspection failed; body withheld') from None


def known_schema(document):
    version = document.get('format_version')
    if not isinstance(version, str) or not re.fullmatch(r'1\.[0-9]+', version):
        raise InspectionError('Unsupported or missing OpenTofu JSON format version')


def report(state, plan, app, token, get=api_get):
    known_schema(state)
    known_schema(plan)
    identity(app, 'app')
    if plan.get('errored') or plan.get('complete') is False or plan.get('deferred_changes'):
        raise InspectionError('Incomplete inspection plan')
    snapshots = {'persisted': selected(state.get('values', {}), app),
                 'refreshed': selected(plan.get('prior_state', {}).get('values', {}), app),
                 'planned': selected(plan.get('planned_values', {}), app)}
    machine_ids = {row['machine_id'] for row in snapshots.values()}
    if len(machine_ids) != 1:
        raise InspectionError('Machine identity differs across snapshots; no recovery inferred')
    machine_id = snapshots['refreshed']['machine_id']
    machine = get('apps/' + app + '/machines/' + machine_id, token)
    if identity(machine.get('id'), 'machine') != machine_id:
        raise InspectionError('Fly machine response identity mismatch')
    mounts = machine.get('config', {}).get('mounts', [])
    if len(mounts) != 1 or mounts[0].get('path') != '/data':
        raise InspectionError('Fly machine authority mount is ambiguous')
    live_id = identity(mounts[0].get('volume'), 'volume')
    volume_rows = get('apps/' + app + '/volumes', token)
    if not isinstance(volume_rows, list):
        raise InspectionError('Invalid Fly volume inventory')
    wanted = {live_id} | {row[key] for row in snapshots.values()
                         for key in ('volume_resource_id', 'mounted_volume_id')}
    inventory = []
    for volume_id in sorted(wanted):
        matches = [v for v in volume_rows if v.get('id') == volume_id]
        if len(matches) > 1:
            raise InspectionError('Duplicate volume identity in Fly inventory')
        row = {'volume_id': volume_id, 'exists': bool(matches)}
        if matches:
            volume = matches[0]
            row['attached_machine_id'] = (identity(volume['attached_machine_id'], 'machine')
                                         if volume.get('attached_machine_id') else None)
            row['authority_name_matches'] = volume.get('name') == 'live_authority'
            row['machine_region_matches'] = volume.get('region') == machine.get('region')
        inventory.append(row)
    for row in snapshots.values():
        row['resource_matches_live_mount'] = row['volume_resource_id'] == live_id
        row['mount_matches_live_mount'] = row['mounted_volume_id'] == live_id
    return {'schema': 1, 'operation': 'read_only_no_recovery_performed',
            'live_machine_id': machine_id, 'live_mounted_volume_id': live_id,
            'snapshots': snapshots, 'volume_inventory': inventory}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('state', type=Path)
    parser.add_argument('plan', type=Path)
    args = parser.parse_args()
    try:
        app = os.environ['LIVE_FLY_APP']
        token = os.environ['FLY_API_TOKEN']
        result = report(json.loads(args.state.read_text()), json.loads(args.plan.read_text()), app, token)
        print(json.dumps(result, indent=2, sort_keys=True))
    except InspectionError as error:
        print('Ownership inspection blocked: ' + str(error), file=sys.stderr)
        return 1
    except (OSError, ValueError, TypeError, AttributeError, KeyError, RecursionError):
        print('Ownership inspection blocked: invalid input; raw values withheld', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
