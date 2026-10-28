#!/usr/bin/env python3
"""Scan exact monitoring images; start only after all scans pass. No persisted bypass."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
CONFIG = ROOT / 'infra/live-monitoring'


def checked_images(images, run=subprocess.run):
    if set(images) != {'grafana', 'prometheus', 'alertmanager'}:
        raise ValueError('expected exactly three monitoring images')
    env = dict(os.environ)
    for name, image in images.items():
        if not re.fullmatch(r'[a-z0-9./_-]+:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}', image):
            raise ValueError('image must have a version and immutable SHA-256 digest')
        # No ignore files, ignored-unfixed filter, or severity suppression. Scanner
        # errors also fail closed. Registry IO only; no Docker daemon required.
        result = run(['trivy', '--config', '/dev/null', 'image', '--image-src', 'remote',
                      '--scanners', 'vuln', '--severity', 'HIGH,CRITICAL',
                      '--ignorefile', '/dev/null', '--exit-code', '1', image],
                     check=False, env={k: v for k, v in os.environ.items()
                                       if not k.startswith('TRIVY_')})
        if result.returncode != 0:
            raise RuntimeError(f'{name}: vulnerability scan failed; monitoring startup blocked')
        env['MONITORING_' + name.upper() + '_IMAGE'] = image
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--up', action='store_true', help='explicitly start monitoring after successful scans')
    args = parser.parse_args()
    try:
        env = checked_images(json.loads((CONFIG / 'images.json').read_text()))
        if args.up:
            subprocess.run(['docker', 'compose', '-f', str(CONFIG / 'compose.json'), 'up', '-d'],
                           env=env, check=True)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
