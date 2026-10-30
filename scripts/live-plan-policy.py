#!/usr/bin/env python3
"""Fail closed on unreviewed, destructive or unknown infrastructure changes."""
import argparse
import hashlib
import hmac
import json
import os
import re
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


def has_unknown(value):
    """OpenTofu after_unknown is a tree of booleans, not container presence.

    False leaves mean known, including inside nonempty list/block structures.
    Unexpected leaf types fail closed instead of silently counting as known.
    """
    if isinstance(value, dict):
        return any(has_unknown(child) for child in value.values())
    if isinstance(value, list):
        return any(has_unknown(child) for child in value)
    return value is not False


def validate(plan, configure=False, infrastructure=False):
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
        if address == 'fly_machine.live' and actions == ['update']:
            before, after = change.get('before'), change.get('after')
            # Fly forbids swapping a mounted volume on an existing machine.
            # Enforce this even for reviewed infrastructure/configure plans.
            # Compare identity/path only: reconciliation flags are not mounts.
            def mounts(value):
                blocks = value.get('mount', []) if isinstance(value, dict) else []
                if not isinstance(blocks, list) or any(not isinstance(block, dict) for block in blocks):
                    raise PolicyRejection('Invalid machine mount configuration')
                return [(block.get('path'), block.get('volume')) for block in blocks]
            unknown = change.get('after_unknown', {})
            if not isinstance(unknown, dict):
                raise PolicyRejection('Invalid machine unknown configuration')
            if has_unknown(unknown.get('mount', False)) or mounts(before) != mounts(after):
                raise PolicyRejection('In-place authority mount change rejected; inspect host-migration reconciliation')
        if configure or infrastructure:
            continue
        if address not in RELEASE_RESOURCES or actions != ['update'] or change.get('importing'):
            raise PolicyRejection('Infrastructure setup or drift detected; configure first')
        # Ordinary releases may change the immutable image only, not authority,
        # ports, region, guest size, environment, mounts or machine ownership.
        before, after = change.get('before'), change.get('after')
        if not isinstance(before, dict) or not isinstance(after, dict):
            raise PolicyRejection('Missing machine configuration')
        unknown = change.get('after_unknown', {})
        if not isinstance(unknown, dict) or any(has_unknown(value) for key, value in unknown.items()
                                               if key not in ('id', 'instance_id', 'private_ip', 'state')):
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


def plan_receipt(plan):
    """Bind review to exact proposed changes, candidate and workflow activation.

    A keyed digest lets a public job output attest private plan values without
    exposing them or enabling offline guesses of low-entropy configuration.
    """
    key = os.environ.get('TOFU_STATE_ENCRYPTION_PASSPHRASE', '')
    context = {name: os.environ.get(name, '') for name in
               ('GITHUB_REPOSITORY', 'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_SHA')}
    if not key or not all(context.values()):
        raise PolicyRejection('Missing approval receipt context')
    payload = {'context': context, 'plan': {name: plan.get(name) for name in
               ('resource_changes', 'output_changes', 'variables', 'planned_values', 'configuration')}}
    encoded = json.dumps(payload, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()
    return hmac.new(key.encode(), b'bmux-reviewed-plan-v1\0' + encoded, hashlib.sha256).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('plan', type=Path)
    parser.add_argument('--configure', action='store_true')
    parser.add_argument('--infrastructure', action='store_true', help='Known non-destructive changes, no imports')
    parser.add_argument('--review', choices=('preview', 'apply'))
    args = parser.parse_args()
    plan = None
    try:
        plan = json.loads(args.plan.read_text())
        validate(plan, args.configure, args.infrastructure)
        if args.review:
            receipt = plan_receipt(plan)
            if args.review == 'apply':
                expected = os.environ.get('LIVE_APPROVED_PLAN', '')
                if not re.fullmatch('[0-9a-f]{64}', expected) or not hmac.compare_digest(expected, receipt):
                    raise PolicyRejection('Plan changed after preview; a new preview and approval are required')
            else:
                with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
                    output.write(f'plan_receipt={receipt}\n')
                summary = public_change_summary(plan) or '  No resource changes'
                with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as output:
                    output.write('## Production deployment preview\n\nKnown non-destructive changes; '
                                 'imports, deletes and replacements are blocked.\n\n```text\n' + summary + '\n```\n')
    except PolicyRejection as exc:
        # Only deliberately authored policy messages and fixed inventory labels
        # are public. Never print raw addresses, attribute names, or values.
        summary = public_change_summary(plan)
        detail = f'\nChanged resources (configuration values suppressed):\n{summary}\n' if summary else ''
        parser.exit(2, f'Infrastructure plan rejected: {exc}. Plan values suppressed.\n{detail}')
    except (OSError, ValueError, TypeError, AttributeError, KeyError):
        parser.exit(2, 'Infrastructure plan rejected; invalid or unreadable plan. Plan values suppressed.\n')


if __name__ == '__main__':
    main()
