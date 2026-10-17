#!/usr/bin/env python3
"""Inspect a private local copy of canonical state; remote operations are reads only."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('archive_live_state', ROOT / 'scripts/archive-live-state.py')
STORE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STORE)
ADDRESSES = ('fly_app.live', 'fly_machine.live', 'fly_volume.authority',
             'cloudflare_worker.live', 'cloudflare_worker_version.live',
             'cloudflare_workers_deployment.live', 'cloudflare_workers_route.live')


def inspect(store, runner=subprocess.run):
    store.unlocked()
    marker = store.migration_marker()
    body, etag = store.get(store.key)
    # Reject plaintext/unsupported encrypted envelopes using the existing authority checks.
    STORE.encrypted(body)
    with tempfile.TemporaryDirectory(prefix='bmux-read-only-state-') as temporary:
        directory = Path(temporary)
        directory.chmod(0o700)
        (directory / 'encryption.tf').write_bytes((ROOT / 'infra/live-terraform/encryption.tf').read_bytes())
        (directory / 'backend.tf').write_text('terraform {\n  backend "local" {}\n}\n')
        state = directory / 'terraform.tfstate'
        # Install the copied state only after local backend initialization.
        # Init otherwise tries to read it before encryption configuration loads.
        # No remote backend or provider configuration. Decryption and state list
        # operate only on the downloaded encrypted copy, never the R2 backend.
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('AWS_', 'TF_', 'TOFU_', 'R2_', 'CLOUDFLARE_', 'FLY_'))}
        env['TF_VAR_state_encryption_passphrase'] = os.environ['TOFU_STATE_ENCRYPTION_PASSPHRASE']
        env['TF_DATA_DIR'] = str(directory / 'tofu')
        env['TF_IN_AUTOMATION'] = 'true'
        for command in (['tofu', 'init', '-input=false', '-no-color'],
                        ['tofu', 'state', 'list']):
            if command[1] == 'state':
                state.write_bytes(body)
                state.chmod(0o600)
            result = runner(command, cwd=directory, env=env, capture_output=True, text=True, timeout=90)
            if result.returncode:
                raise ValueError('Local state inspection failed; raw diagnostics suppressed')
        resources = result.stdout.splitlines()
        if any(not line or len(line) > 512 for line in resources):
            raise ValueError('Invalid state address list')
    again, again_etag = store.get(store.key)
    store.unlocked()
    if body != again or etag != again_etag or marker != store.migration_marker():
        raise ValueError('Canonical state or migration marker changed during inspection')
    return {'read_only': True, 'migration_phase': marker[0]['phase'] if marker else 'absent',
            'managed_resources': {address: address in resources for address in ADDRESSES}}


def main():
    os.umask(0o077)
    store = STORE.Store()
    try:
        report = inspect(store)
        text = json.dumps(report, indent=2, sort_keys=True)
        print(text)
        if os.environ.get('GITHUB_STEP_SUMMARY'):
            with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as stream:
                stream.write('## Read-only migration inspection\n```json\n' + text + '\n```\n')
    finally:
        store.temp.cleanup()


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, AttributeError, STORE.StateError, subprocess.SubprocessError):
        raise SystemExit('Read-only inspection failed; canonical state unchanged. Private details suppressed.') from None
