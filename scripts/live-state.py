#!/usr/bin/env python3
"""Render the canonical R2 S3-protocol backend, never credentials."""
import argparse
import json
import os
from pathlib import Path
import re


def config(environment, bucket, region, account_id):
    if environment not in ('staging', 'production'):
        raise ValueError('Unsupported environment')
    if not re.fullmatch(r'[a-z0-9][a-z0-9-]{1,61}[a-z0-9]', bucket):
        raise ValueError('Invalid R2 bucket')
    if region != 'auto' or not re.fullmatch(r'[a-f0-9]{32}', account_id):
        raise ValueError('Legacy AWS backend rejected; manually migrate canonical state before selecting R2')
    return dict(bucket=bucket, region='auto', key=f'bmux/live/{environment}/terraform.tfstate',
                endpoint=f'https://{account_id}.r2.cloudflarestorage.com', use_lockfile=True,
                skip_credentials_validation=True, skip_metadata_api_check=True,
                skip_region_validation=True, skip_requesting_account_id=True, skip_s3_checksum=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--environment', choices=('staging', 'production'), required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        value = config(args.environment, os.environ['LIVE_STATE_BUCKET'],
                       os.environ.get('LIVE_STATE_REGION', 'auto'), os.environ['CLOUDFLARE_ACCOUNT_ID'])
    except (KeyError, ValueError):
        parser.error('Set R2 LIVE_STATE_BUCKET, CLOUDFLARE_ACCOUNT_ID and region auto; legacy AWS requires manual migration')
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(value, stream)
        stream.write('\n')


if __name__ == '__main__':
    main()
