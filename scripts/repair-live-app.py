#!/usr/bin/env python3
"""Explicit app-taint repair; encrypted archive, exclusive lock, conditional publication."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('inspection', ROOT / 'scripts/inspect-live-migration.py')
INSPECT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSPECT)
STORE = INSPECT.STORE


def verify(state, live, app, org):
    status = INSPECT.app_status(state, {'LIVE_FLY_APP': app, 'LIVE_FLY_ORG': org})
    if not status.get('present') or not status.get('name_matches_configuration') or not status.get('org_matches_configuration'):
        raise ValueError('State app identity/configuration mismatch')
    resource = next(r for r in state['resources'] if r.get('mode') == 'managed'
                    and r.get('type') == 'fly_app' and r.get('name') == 'live' and not r.get('module'))
    attributes = resource['instances'][0]['attributes']
    if (not isinstance(live, dict) or live.get('name') != app or not live.get('id')
            or live['id'] != attributes.get('id') or live.get('organization', {}).get('slug') != org):
        raise ValueError('Live durable app identity/configuration mismatch')
    if not isinstance(state.get('serial'), int) or not isinstance(state.get('lineage'), str) or not state['lineage']:
        raise ValueError('Missing state revision/authority')
    return status['tainted']


def publish(store, before, etag, marker, after):
    STORE.encrypted(after)
    store.unlocked()
    lock = json.dumps(dict(ID=str(uuid.uuid4()), Operation='OperationTypeStatePush',
                           Info='Explicit verified app untaint', Who='bmux-recovery',
                           Version='1.10.9', Path=store.key)).encode()
    lock_etag = store.put(store.key + '.tflock', lock, '--if-none-match', '*').get('ETag')
    if not STORE.migration_etag(lock_etag):
        raise ValueError('Ambiguous lock; retain for reconciliation')
    # Failures after acquisition deliberately retain the lock. Never blindly retry.
    if store.get(store.key, etag) != (before, etag) or store.migration_marker() != marker:
        raise ValueError('Authority changed; retain recovery lock')
    archive = store.prefix + hashlib.sha256(before).hexdigest() + '-' + uuid.uuid4().hex + '.tfstate'
    store.put(archive, before, '--if-none-match', '*')
    if store.get(archive)[0] != before:
        raise ValueError('Archive verification failed; retain recovery lock')
    store.put(store.key, after, '--if-match', etag)
    if store.get(store.key)[0] != after or store.migration_marker() != marker:
        raise ValueError('Ambiguous publication; retain recovery lock')
    store.call('delete-object', '--key', store.key + '.tflock', '--if-match', lock_etag)


def repair(store, apply):
    store.unlocked()
    marker = store.migration_marker()
    if marker is None or marker[0]['phase'] != 'applying':
        raise ValueError('Interrupted migration required')
    before, etag = store.get(store.key)
    with tempfile.TemporaryDirectory(prefix='bmux-app-repair-') as temporary:
        directory = Path(temporary)
        directory.chmod(0o700)
        (directory / 'encryption.tf').write_bytes((ROOT / 'infra/live-terraform/encryption.tf').read_bytes())
        (directory / 'backend.tf').write_text('terraform {\n backend "local" {}\n}\n')
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(('AWS_', 'TF_', 'TOFU_', 'R2_', 'CLOUDFLARE_', 'FLY_'))}
        env['TF_VAR_state_encryption_passphrase'] = os.environ['TOFU_STATE_ENCRYPTION_PASSPHRASE']
        env['TF_DATA_DIR'] = str(directory / 'tofu')
        def run(*args):
            result = subprocess.run(['tofu', *args], cwd=directory, env=env,
                                    capture_output=True, text=True, timeout=90)
            if result.returncode:
                raise ValueError('Local OpenTofu operation failed; private output suppressed')
            return result.stdout
        run('init', '-input=false', '-no-color')
        path = directory / 'terraform.tfstate'
        path.write_bytes(before)
        path.chmod(0o600)
        state = json.loads(run('state', 'pull'))
        request = urllib.request.Request('https://api.machines.dev/v1/apps/' + os.environ['LIVE_FLY_APP'],
                     headers={'Authorization': 'Bearer ' + os.environ['FLY_API_TOKEN']})
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read(65537)
            if len(raw) > 65536:
                raise ValueError('Oversized Fly response')
            live = json.loads(raw)
        tainted = verify(state, live, os.environ['LIVE_FLY_APP'], os.environ['LIVE_FLY_ORG'])
        if not apply or not tainted:
            return {'verified_durable_identity': True, 'tainted': tainted, 'changed': False}
        print('Repair phase: local-untaint', flush=True)
        run('untaint', '-no-color', 'fly_app.live')
        after_state = json.loads(run('state', 'pull'))
        expected = json.loads(json.dumps(state))
        resource = next(r for r in expected['resources'] if r.get('mode') == 'managed'
                        and r.get('type') == 'fly_app' and r.get('name') == 'live' and not r.get('module'))
        resource['instances'][0].pop('status', None)
        expected['serial'] += 1
        # OpenTofu stamps the writer version even when the input state came
        # from another compatible release. Validate it against the actual tool,
        # never against an arbitrary value supplied by the changed state.
        version = json.loads(run('version', '-json'))['terraform_version']
        if version != '1.10.9':
            raise ValueError('Unapproved OpenTofu writer version')
        expected['terraform_version'] = version
        print('Repair phase: verify-local-delta', flush=True)
        if after_state != expected:
            raise ValueError('Unexpected state mutation; canonical state unchanged')
        print('Repair phase: conditional-publication', flush=True)
        publish(store, before, etag, marker, path.read_bytes())
        return {'verified_durable_identity': True, 'tainted': False, 'changed': True}


if __name__ == '__main__':
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apply', action='store_true')
    args = parser.parse_args()
    store = STORE.Store()
    try:
        print(json.dumps(repair(store, args.apply)))
    except (ValueError, OSError, KeyError, TypeError, STORE.StateError, subprocess.SubprocessError):
        raise SystemExit('App repair stopped. Inspect state and retained lock before retry; private details suppressed.') from None
    finally:
        store.temp.cleanup()
