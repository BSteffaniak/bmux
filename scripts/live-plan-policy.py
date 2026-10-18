#!/usr/bin/env python3
"""Fail closed on infrastructure drift; ordinary image releases and explicit non-destructive configuration."""
import argparse
import json
from pathlib import Path

RELEASE_RESOURCES = {'fly_machine.live'}
CONFIGURE_RESOURCES = RELEASE_RESOURCES | {
    'fly_app.live', 'fly_volume.authority', 'fly_ip.v4', 'fly_ip.v6',
    'fly_cert.live', 'fly_cert_validation.live',
    'cloudflare_pages_project.live', 'cloudflare_pages_domain.live',
    'cloudflare_dns_record.live', 'cloudflare_dns_record.relay',
    'cloudflare_dns_record.relay_acme', 'cloudflare_r2_bucket.state',
    'cloudflare_r2_bucket_lock.state', 'cloudflare_r2_bucket_lifecycle.state',
}


class PolicyRejection(ValueError):
    """A fixed, public policy reason; never include provider-controlled values."""


def validate(plan, configure=False):
    if not isinstance(plan, dict) or not isinstance(plan.get('resource_changes', []), list):
        raise PolicyRejection('Invalid infrastructure plan')
    if plan.get('errored') or plan.get('complete') is False or plan.get('deferred_changes'):
        raise PolicyRejection('Incomplete infrastructure plan')
    for resource in plan.get('resource_changes', []):
        address = resource.get('address')
        change = resource.get('change', {})
        actions = change.get('actions')
        if not isinstance(address, str) or actions not in (
                ['no-op'], ['read'], ['create'], ['update'], ['delete'],
                ['create', 'delete'], ['delete', 'create']):
            raise PolicyRejection('Unsupported infrastructure action')
        if (change.get('importing') and not configure) or resource.get('deposed'):
            raise PolicyRejection('Unexpected import or deposed resource')
        if address in ('cloudflare_worker.live', 'cloudflare_worker_version.live',
                       'cloudflare_workers_deployment.live', 'cloudflare_workers_route.live'):
            raise PolicyRejection('Legacy Worker ownership requires separate reviewed recovery')
        if actions in (['no-op'], ['read']):
            continue
        if address not in CONFIGURE_RESOURCES or 'delete' in actions:
            raise PolicyRejection('Unexpected resource or destructive change requires separate review')
        if configure:
            continue
        if address not in RELEASE_RESOURCES or actions != ['update'] or change.get('importing'):
            raise PolicyRejection('Infrastructure setup or drift detected; configure first')
        # Ordinary releases may change the immutable image only, not authority,
        # ports, region, guest size, environment, mounts or machine ownership.
        before, after = change.get('before'), change.get('after')
        if not isinstance(before, dict) or not isinstance(after, dict):
            raise PolicyRejection('Missing machine configuration')
        unknown = change.get('after_unknown', {})
        if any(value for key, value in unknown.items() if key not in ('id', 'instance_id', 'private_ip', 'state')):
            raise PolicyRejection('Unknown machine configuration')
        ignored = {'image', 'id', 'instance_id', 'private_ip', 'state'}
        if {k: v for k, v in before.items() if k not in ignored} != {
                k: v for k, v in after.items() if k not in ignored}:
            raise PolicyRejection('Machine infrastructure drift detected')
    return True


def public_change_summary(plan):
    """Bounded inventory from fixed vocabulary only, never provider values.

    A generic "configure first" message is insufficient to review a configure
    operation. Report all changed known addresses/actions, not the first blocker;
    unknown addresses and malformed changes remain anonymous and fail closed.
    """
    labels = set()
    resources = plan.get('resource_changes', []) if isinstance(plan, dict) else []
    if not isinstance(resources, list):
        return ''
    for resource in resources:
        if not isinstance(resource, dict):
            labels.add('unrecognized resource: unsupported action')
            continue
        change = resource.get('change', {})
        change = change if isinstance(change, dict) else {}
        actions = change.get('actions')
        if actions in (['no-op'], ['read']):
            continue
        # Select the canonical constant, not any raw string from the plan.
        address = next((known for known in CONFIGURE_RESOURCES
                        if known == resource.get('address')), 'unrecognized resource')
        action = next((label for allowed, label in (
            (['create'], 'create'), (['update'], 'update'), (['delete'], 'delete'),
            (['create', 'delete'], 'replace'), (['delete', 'create'], 'replace'))
            if actions == allowed), 'unsupported action')
        labels.add(f'{address}: {action}')
    # Vocabulary is finite; output stays bounded even for hostile input arrays.
    return '\n'.join('  ' + label for label in sorted(labels))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('plan', type=Path)
    parser.add_argument('--configure', action='store_true')
    args = parser.parse_args()
    plan = None
    try:
        plan = json.loads(args.plan.read_text())
        validate(plan, args.configure)
    except PolicyRejection as exc:
        # Only deliberately authored policy messages and fixed inventory labels
        # are public. Never print raw addresses, attribute names, or values.
        summary = public_change_summary(plan)
        detail = f'\nChanged resources (configuration values suppressed):\n{summary}\n' if summary else ''
        parser.exit(2, f'Infrastructure plan rejected: {exc}. Plan values suppressed.\n{detail}')
    except (OSError, ValueError, TypeError, AttributeError):
        parser.exit(2, 'Infrastructure plan rejected; invalid or unreadable plan. Plan values suppressed.\n')


if __name__ == '__main__':
    main()
