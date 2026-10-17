#!/usr/bin/env python3
"""Install explicitly supplied Fly credentials; no cloud resource creation.

This is separate from the canonical R2 bootstrap. Never store token values in
resume files. Existing secrets are never replaced implicitly.
"""
import argparse
import getpass
import hashlib
import json
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default='BSteffaniak/bmux')
    parser.add_argument('--approve', action='store_true')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', args.repo):
        parser.error('Invalid repository')
    try:
        result = subprocess.run(['gh', 'secret', 'list', '--repo', args.repo, '--env', 'Production', '--json', 'name'],
                                check=True, capture_output=True)
        names = {x['name'] for x in json.loads(result.stdout)}
        missing = {'FLY_API_TOKEN', 'LIVE_METRICS_KEY_SHA256'} - names
        print('Missing Production secrets: ' + (', '.join(sorted(missing)) or 'none'))
        print('No Fly resources, R2 state, or credentials will be created or rotated.')
        if not args.approve or not missing:
            return
        values = {}
        if 'FLY_API_TOKEN' in missing:
            token = getpass.getpass('Existing Fly organization token (private): ')
            if not token or '\n' in token:
                raise ValueError('Invalid token')
            values['FLY_API_TOKEN'] = token
        if 'LIVE_METRICS_KEY_SHA256' in missing:
            key = getpass.getpass('Independent existing metrics key (private, at least 32 characters): ')
            if len(key) < 32 or key != getpass.getpass('Confirm metrics key: '):
                raise ValueError('Invalid metrics key')
            values['LIVE_METRICS_KEY_SHA256'] = hashlib.sha256(key.encode()).hexdigest()
        for name, value in values.items():
            subprocess.run(['gh', 'secret', 'set', name, '--repo', args.repo, '--env', 'Production'],
                           input=value.encode(), check=True, capture_output=True)
        print('Missing Fly secrets installed. Keep original keys independently; no deployment dispatched.')
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError):
        parser.exit(2, 'Fly credential installation failed; diagnostics suppressed. Rerun to inspect missing names.\n')


if __name__ == '__main__':
    main()
