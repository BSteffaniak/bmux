#!/usr/bin/env python3
"""Reject unexpected infrastructure changes during ordinary live releases."""
import argparse
import json
from pathlib import Path

RELEASE_RESOURCES = {
    'cloudflare_worker_version.live',
    'cloudflare_workers_deployment.live',
}


def validate(plan, configure=False, migration=False):
    if migration and not configure:
        raise ValueError('Migration deployment is configure-only')
    if not isinstance(plan, dict) or not isinstance(plan.get('resource_changes', []), list):
        raise ValueError('Invalid infrastructure plan')
    for resource in plan.get('resource_changes', []):
        address = resource.get('address')
        change = resource.get('change', {})
        actions = change.get('actions')
        if not isinstance(address, str) or not isinstance(actions, list) or not actions:
            raise ValueError('Invalid infrastructure change')
        if any(action not in ('no-op', 'read', 'create', 'update', 'delete') for action in actions):
            raise ValueError('Unsupported infrastructure action')
        version_replacement = address == 'cloudflare_worker_version.live' and actions in (
            ['delete', 'create'], ['create', 'delete'])
        deployment_replacement = address == 'cloudflare_workers_deployment.live' and actions == ['create', 'delete']
        release_replacement = version_replacement or deployment_replacement
        if 'delete' in actions and not release_replacement:
            raise ValueError('Destructive infrastructure changes require a separate reviewed migration')
        if migration:
            if address not in RELEASE_RESOURCES | {'cloudflare_worker.live'}:
                raise ValueError('Migration stage must not include routes or unrelated resources')
            after = change.get('after') or {}
            if address == 'cloudflare_worker_version.live':
                migration_values = {key: value for key, value in (after.get('migrations') or {}).items()
                                    if value is not None}
                if (after.get('bindings') != [] or migration_values != {
                        'new_tag': 'v1', 'new_sqlite_classes': ['LiveChannel']}):
                    raise ValueError('Migration version must introduce v1 without bindings')
        if not configure and change.get('importing'):
            raise ValueError('Resource adoption is restricted to infrastructure configuration')
        if not configure and (actions not in (['no-op'], ['read'])):
            if address not in RELEASE_RESOURCES or ('create' in actions and not release_replacement) or change.get('importing'):
                raise ValueError('Infrastructure setup or drift detected; run Configure Production Infrastructure first')
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('plan', type=Path)
    parser.add_argument('--configure', action='store_true')
    parser.add_argument('--migration', action='store_true')
    args = parser.parse_args()
    try:
        validate(json.loads(args.plan.read_text()), args.configure, args.migration)
    except (OSError, ValueError, TypeError, AttributeError):
        parser.exit(2, 'Infrastructure plan rejected; configure infrastructure or review a separate migration. Plan values suppressed.\n')


if __name__ == '__main__':
    main()
