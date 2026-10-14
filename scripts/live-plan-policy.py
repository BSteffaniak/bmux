#!/usr/bin/env python3
"""Reject unexpected infrastructure changes during ordinary live releases."""
import argparse
import json
from pathlib import Path

RELEASE_RESOURCES = {
    'cloudflare_worker_version.live',
    'cloudflare_workers_deployment.live',
}


def validate(plan, configure=False):
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
        if 'delete' in actions and not version_replacement:
            raise ValueError('Destructive infrastructure changes require a separate reviewed migration')
        if not configure and change.get('importing'):
            raise ValueError('Resource adoption is restricted to infrastructure configuration')
        if not configure and (actions not in (['no-op'], ['read'])):
            if address not in RELEASE_RESOURCES or ('create' in actions and not version_replacement) or change.get('importing'):
                raise ValueError('Infrastructure setup or drift detected; run Configure Production Infrastructure first')
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('plan', type=Path)
    parser.add_argument('--configure', action='store_true')
    args = parser.parse_args()
    try:
        validate(json.loads(args.plan.read_text()), args.configure)
    except (OSError, ValueError, TypeError, AttributeError):
        parser.exit(2, 'Infrastructure plan rejected; configure infrastructure or review a separate migration. Plan values suppressed.\n')


if __name__ == '__main__':
    main()
