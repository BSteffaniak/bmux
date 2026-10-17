#!/usr/bin/env python3
"""Read-only Fly collision check; never adopt another stack's app implicitly."""
import argparse
import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.request


def inspect(managed, app, token, opener=urllib.request.urlopen):
    if not isinstance(managed, list) or any(not isinstance(x, str) for x in managed):
        raise ValueError('Invalid managed inventory')
    if not re.fullmatch('[a-z0-9][a-z0-9-]{0,62}', app) or not token:
        raise ValueError('Invalid Fly configuration')
    request = urllib.request.Request('https://api.machines.dev/v1/apps/' + app,
                                     headers={'Authorization': 'Bearer ' + token})
    try:
        with opener(request, timeout=30) as response:
            data = response.read(65537)
            if len(data) > 65536:
                raise ValueError('Oversized Fly response')
            result = json.loads(data)
            if not isinstance(result, dict) or result.get('name') != app:
                raise ValueError('Invalid Fly identity')
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise ValueError('Fly inventory rejected') from None
        if 'fly_app.live' in managed:
            raise ValueError('Managed Fly app missing; explicit recovery required') from None
        return
    if 'fly_app.live' not in managed:
        raise ValueError('Fly app already exists outside this state; explicit reviewed import required')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('managed', type=Path)
    args = parser.parse_args()
    try:
        inspect(json.loads(args.managed.read_text()), os.environ['LIVE_FLY_APP'], os.environ['FLY_API_TOKEN'])
    except (OSError, ValueError, KeyError, TypeError):
        parser.exit(2, 'Fly ownership preflight rejected; inspect protected inventory. Remote values suppressed.\n')


if __name__ == '__main__':
    main()
