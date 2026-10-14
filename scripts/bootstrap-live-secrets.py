#!/usr/bin/env python3
"""Bootstrap targeted live GitHub environment secrets; no secret values in diagnostics."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys


class BootstrapError(Exception):
    """Sanitized error; provider diagnostics are never printed."""


SECRET_NAMES = (
    'CLOUDFLARE_API_TOKEN', 'CLOUDFLARE_ACCOUNT_ID', 'CLOUDFLARE_ZONE_ID',
    'LIVE_HOSTNAME', 'LIVE_PAGES_PROJECT', 'LIVE_PUBLISHER_KEY_SHA256',
    'LIVE_STATE_BUCKET', 'LIVE_STATE_REGION', 'LIVE_STATE_ACCESS_KEY_ID',
    'LIVE_STATE_SECRET_ACCESS_KEY',
)


def command(args, body=None):
    try:
        result = subprocess.run(args, input=body, text=True, capture_output=True,
                                check=False, timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        raise BootstrapError('GitHub operation unavailable or timed out; inspect state before retry') from None
    if result.returncode:
        raise BootstrapError('GitHub operation failed; output redacted; inspect state before retry')
    return result.stdout


def read_private_key(path):
    # No symlinks, no group/world permissions; open the actual verified descriptor.
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_uid != os.getuid():
                raise BootstrapError('Publisher key must be an owned private regular file (0600)')
            key = stream.read(4097).strip()
    except OSError:
        raise BootstrapError('Cannot read private publisher key') from None
    if not 32 <= len(key) <= 4096 or any(c <= 32 or c >= 127 for c in key):
        raise BootstrapError('Publisher key must be 32–4096 printable ASCII bytes without whitespace')
    return hashlib.sha256(key).hexdigest()


def token_policy(account, zone, permission_groups):
    """Separate account Workers/Pages scope from zone-route scope; no broad zone wildcard."""
    policies = []
    for scope, resource, names in (
        ('com.cloudflare.api.account', f'com.cloudflare.api.account.{account}',
         ('Workers Scripts Write', 'Pages Write')),
        ('com.cloudflare.api.account.zone', f'com.cloudflare.api.account.zone.{zone}',
         ('Workers Routes Write', 'DNS Write')),
    ):
        ids = []
        for name in names:
            matches = [g for g in permission_groups if g.get('name') == name and scope in g.get('scopes', [])]
            if len(matches) != 1:
                raise BootstrapError('Cannot uniquely resolve required scoped Cloudflare permission')
            ids.append({'id': matches[0]['id']})
        policies.append({'effect': 'allow', 'resources': {resource: '*'}, 'permission_groups': ids})
    return {'policies': policies}


def verify_environment(repo, environment, run=command):
    data = json.loads(run(['gh', 'api', f'repos/{repo}/environments/{environment}']))
    # Never PUT the environment: do not accidentally replace/erase existing protections.
    policy = data.get('deployment_branch_policy') or {}
    if not policy.get('custom_branch_policies'):
        raise BootstrapError('Environment needs an existing custom branch policy restricted to master')
    branches = json.loads(run(['gh', 'api', f'repos/{repo}/environments/{environment}/deployment-branch-policies']))
    rules = branches.get('branch_policies', [])
    if not rules or any(r.get('name') != 'master' or r.get('type', 'branch') != 'branch' for r in rules):
        raise BootstrapError('Environment branch policy must allow only master (no tags/wildcards)')
    if environment == 'production':
        reviews = [r for r in data.get('protection_rules', []) if r.get('type') == 'required_reviewers']
        if not reviews or not reviews[0].get('reviewers') or not reviews[0].get('prevent_self_review'):
            raise BootstrapError('Production needs required reviewers and prevent_self_review')
        if data.get('can_admins_bypass', True):
            raise BootstrapError('Production must disallow admin protection bypass')


def provision(args, run=command, environ=None):
    environ = os.environ if environ is None else environ
    selected = args.only or list(SECRET_NAMES)
    if args.dry_run:
        print(json.dumps({'repo': args.repo, 'environment': args.environment,
                          'mode': 'replace-selected' if args.rotate else 'skip-existing',
                          'secrets': selected, 'provider_requests': False,
                          'protections': 'verify only; never weaken'}))
        return
    verify_environment(args.repo, args.environment, run)
    existing = {s['name'] for s in json.loads(run([
        'gh', 'secret', 'list', '--repo', args.repo, '--env', args.environment, '--json', 'name']))}
    pending = [name for name in selected if args.rotate or name not in existing]
    values = {}
    # Validate every input before any mutation; publisher key never goes to GitHub.
    for name in pending:
        if name == 'LIVE_PUBLISHER_KEY_SHA256':
            if not args.publisher_key_file:
                raise BootstrapError('--publisher-key-file is required for a new digest')
            value = read_private_key(args.publisher_key_file)
        else:
            value = environ.get(name)
        if not value or '\n' in value or '\r' in value:
            raise BootstrapError(f'Missing/invalid input for {name}')
        values[name] = value
    if args.rotate and not args.only:
        raise BootstrapError('Rotation requires explicit --only selections; no blanket rotation')
    for name, value in values.items():
        run(['gh', 'secret', 'set', name, '--repo', args.repo, '--env', args.environment], value)
        print(f'Set {name} in selected environment')
    for name in selected:
        if name not in values:
            print(f'Skipped existing {name}')
    # No automatic old-token revocation: CI in flight may still hold old credentials.
    if args.rotate:
        print('Rotation installed; verify a protected deployment before manually revoking old credentials')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--environment', choices=('staging', 'production'), required=True)
    parser.add_argument('--only', action='append', choices=SECRET_NAMES)
    parser.add_argument('--publisher-key-file', type=Path)
    parser.add_argument('--rotate', action='store_true')
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo):
        parser.error('--repo must be OWNER/REPO')
    try:
        provision(args)
    except BootstrapError as error:
        print(f'ERROR: {error}', file=sys.stderr)
        return 1
    except (ValueError, KeyError, TypeError):
        print('ERROR: Invalid GitHub response; inspect state before retry', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
