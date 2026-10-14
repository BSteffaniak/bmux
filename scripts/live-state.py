#!/usr/bin/env python3
"""Render explicit AWS S3 backend configuration; never include credentials."""
import argparse
import json
import os
from pathlib import Path
import re


def config(environment, bucket, region):
    if environment not in ('staging', 'production'):
        raise ValueError('Unsupported environment')
    if not re.fullmatch(r'[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]', bucket):
        raise ValueError('Invalid S3 bucket')
    if not re.fullmatch(r'[a-z]{2}-[a-z]+-\d', region):
        raise ValueError('Invalid AWS region')
    return dict(bucket=bucket, region=region, key=f'bmux/live/{environment}/terraform.tfstate',
                encrypt=True, use_lockfile=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--environment', choices=('staging', 'production'), required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        value = config(args.environment, os.environ['LIVE_STATE_BUCKET'], os.environ['LIVE_STATE_REGION'])
    except (KeyError, ValueError):
        parser.error('Set valid LIVE_STATE_BUCKET and LIVE_STATE_REGION (AWS S3 only)')
    # Exclusive creation avoids overwriting another mutation's backend selection.
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(value, stream)
        stream.write('\n')


if __name__ == '__main__':
    main()
