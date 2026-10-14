#!/usr/bin/env python3
"""Encrypted R2 history/recovery. S3 protocol only; no AWS account operations.

Restore uses conditional PUT and a temporary OpenTofu-compatible lock. The lock
cooperates with OpenTofu; it is not a transaction with unrelated S3 writers.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import selectors
import signal
import stat
import urllib.request
import urllib.error
import time
import sys
import tempfile
import uuid

HERE = Path(__file__).resolve().parent


class StateError(Exception):
    pass


def bounded_run(args, env, cwd=None, timeout=120, limit=32 * 1024 * 1024):
    process = subprocess.Popen(args, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    selector = selectors.DefaultSelector()
    output = {process.stdout: bytearray(), process.stderr: bytearray()}
    for stream in output:
        os.set_blocking(stream.fileno(), False)
        selector.register(stream, selectors.EVENT_READ)
    deadline = time.monotonic() + timeout
    total = 0
    try:
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise StateError('Private tool deadline exceeded')
            for key, _ in selector.select(min(remaining, 0.1)):
                block = os.read(key.fileobj.fileno(), 65536)
                if not block:
                    selector.unregister(key.fileobj)
                    continue
                total += len(block)
                if total > limit:
                    raise StateError('Private tool output limit exceeded')
                output[key.fileobj].extend(block)
        code = process.wait(timeout=max(0.01, deadline - time.monotonic()))
        return subprocess.CompletedProcess(args, code, bytes(output[process.stdout]), bytes(output[process.stderr]))
    finally:
        # Also reap descendants retaining pipe handles or outliving their leader.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        selector.close()
        process.stdout.close()
        process.stderr.close()


def encrypted(body):
    try:
        value = json.loads(body)
        if (not isinstance(value, dict) or value.get('encryption_version') != 'v0'
                or not isinstance(value.get('meta'), dict) or not value['meta']
                or not isinstance(value.get('encrypted_data'), str)
                or any(k in value for k in ('resources', 'outputs'))):
            raise ValueError()
        if len(base64.b64decode(value['encrypted_data'], validate=True)) < 16:
            raise ValueError()
    except (ValueError, TypeError):
        raise StateError('Unknown/plaintext/corrupt state rejected; restore verified encrypted authority') from None
    return body


def read_private(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 1024 * 1024:
            raise StateError('Unsafe durable admission file')
        return json.load(stream)


def save_private(path, value, replace=False):
    target = path.with_name('.' + uuid.uuid4().hex) if replace else path
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(value, stream, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    if replace:
        os.replace(target, path)
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def completion(path, config):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 1024 * 1024:
            raise StateError('Unsafe bootstrap completion file')
        value = json.load(stream)
    if (set(value) != {'schema', 'kind', 'operation', 'backend', 'created', 'bucket_identity', 'binding'}
            or value['schema'] != 1 or value['kind'] != 'bmux-live-r2-bootstrap-completion'
            or value['created'] is not True or not re.fullmatch(r'[a-f0-9]{32}', value['operation'])):
        raise StateError('Invalid new-bucket bootstrap completion')
    binding = value.pop('binding')
    if binding != hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest():
        raise StateError('Bootstrap completion content identity mismatch')
    expected = {k: config[k] for k in ('bucket', 'region', 'key', 'endpoint', 'use_lockfile')}
    expected.update(encrypt=True, account_id=os.environ['CLOUDFLARE_ACCOUNT_ID'])
    if value['backend'] != expected:
        raise StateError('Bootstrap completion selects another backend')
    row = value['bucket_identity']
    if (not isinstance(row, dict) or set(row) != {'name', 'creation_date', 'storage_class'}
            or row['name'] != config['bucket'] or row['storage_class'] != 'Standard'
            or not isinstance(row['creation_date'], str)):
        raise StateError('Invalid created-bucket generation')
    return value


def verify_bucket(config, expected):
    # Canonical endpoint only; do not inherit proxies or follow bearer redirects.
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            raise StateError('Cloudflare redirect rejected')
    url = ('https://api.cloudflare.com/client/v4/accounts/' + os.environ['CLOUDFLARE_ACCOUNT_ID']
           + '/r2/buckets/' + config['bucket'])
    request = urllib.request.Request(url, headers={'Authorization': 'Bearer ' + os.environ['CLOUDFLARE_API_TOKEN']})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=30) as response:
        if response.status != 200 or response.geturl() != url:
            raise StateError('Bucket generation verification failed')
        raw = response.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise StateError('Bucket generation response exceeds bound')
        result = json.loads(raw)
    row = result.get('result')
    if (result.get('success') is not True or not isinstance(row, dict)
            or {k: row.get(k) for k in expected} != expected):
        raise StateError('Created bucket generation changed; never seed replacement authority')


class Store:
    def __init__(self):
        spec = importlib.util.spec_from_file_location('live_state', HERE / 'live-state.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.config = module.config(os.environ['LIVE_ENVIRONMENT'], os.environ['LIVE_STATE_BUCKET'],
                                    os.environ.get('LIVE_STATE_REGION', 'auto'), os.environ['CLOUDFLARE_ACCOUNT_ID'])
        self.key = self.config['key']
        self.prefix = 'history/' + self.key + '/'
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(('AWS_', 'TF_', 'TOFU_'))}
        self.env.update(AWS_ACCESS_KEY_ID=os.environ['R2_STATE_ACCESS_KEY_ID'],
                        AWS_SECRET_ACCESS_KEY=os.environ['R2_STATE_SECRET_ACCESS_KEY'],
                        AWS_REGION='auto', AWS_DEFAULT_REGION='auto', AWS_EC2_METADATA_DISABLED='true',
                        AWS_CONFIG_FILE='/dev/null', AWS_SHARED_CREDENTIALS_FILE='/dev/null', AWS_PAGER='')
        self.temp = tempfile.TemporaryDirectory(prefix='bmux-state-')
        self.directory = Path(self.temp.name)

    def call(self, operation, *args):
        result = bounded_run(['aws', 's3api', operation, '--bucket', self.config['bucket'],
                              '--endpoint-url', self.config['endpoint'], '--region', 'auto',
                              '--output', 'json', *args], self.env)
        if result.returncode or len(result.stdout) > 4 * 1024 * 1024:
            raise StateError('R2 operation failed; output suppressed; reconcile exact identities before retry')
        return json.loads(result.stdout or b'{}')

    def unlocked(self):
        result = self.call('list-objects-v2', '--prefix', self.key + '.tflock', '--max-keys', '2')
        if result.get('IsTruncated'):
            raise StateError('Truncated lock inventory rejected')
        if any(v['Key'] == self.key + '.tflock' for v in result.get('Contents', [])):
            raise StateError('Live OpenTofu lock exists; never force unlock')

    def get(self, key, expected=None):
        path = self.directory / uuid.uuid4().hex
        args = ['--key', key]
        if expected:
            args += ['--if-match', expected]
        result = self.call('get-object', *args, str(path))
        body = encrypted(path.read_bytes())
        if not result.get('ETag') or (expected and result['ETag'] != expected):
            raise StateError('State ETag changed or unavailable')
        return body, result['ETag']

    def put(self, key, body, *conditions):
        path = self.directory / uuid.uuid4().hex
        path.write_bytes(body)
        return self.call('put-object', '--key', key, '--body', str(path), *conditions)

    def admission_key(self):
        return 'bootstrap/' + self.key + '.json'

    def inventory(self):
        result = self.call('list-objects-v2', '--max-keys', '3')
        if result.get('IsTruncated') or not isinstance(result.get('Contents', []), list):
            raise StateError('Incomplete bootstrap inventory')
        return {v['Key'] for v in result.get('Contents', [])}

    def admit(self, receipt, output):
        if self.key != 'bmux/live/production/terraform.tfstate':
            raise StateError('First deployment admission is production-only')
        if output.exists() or output.is_symlink() or not output.is_absolute():
            raise StateError('Admission output must be a new private absolute file')
        value = completion(receipt, self.config)
        verify_bucket(self.config, value['bucket_identity'])
        if self.inventory():
            raise StateError('First deployment requires completely empty confirmed new bucket')
        lock = json.dumps(dict(ID=value['operation'], Operation='OperationTypeApply',
                               Info='One-shot first deployment admission', Who='bmux-bootstrap',
                               Version='1.10.9', Path=self.key)).encode()
        lock_etag = self.put(self.key + '.tflock', lock, '--if-none-match', '*').get('ETag')
        if not lock_etag:
            raise StateError('Admission lock identity unavailable; retain lock')
        if self.inventory() != {self.key + '.tflock'}:
            raise StateError('Bucket changed during admission; retain lock')
        verify_bucket(self.config, value['bucket_identity'])
        marker = dict(schema=1, receipt=value, phase='admitted')
        body = json.dumps(marker, sort_keys=True).encode()
        etag = self.put(self.admission_key(), body, '--if-none-match', '*').get('ETag')
        if not etag:
            raise StateError('Admission marker outcome ambiguous; retain lock')
        # Remote permanent consumption precedes local acknowledgement. A lost
        # local receipt requires explicit recovery, never a new empty lineage.
        save_private(output, dict(schema=1, marker=marker, etag=etag))
        self.call('delete-object', '--key', self.key + '.tflock', '--if-match', lock_etag)
        return {'admitted_operation': value['operation']}

    def validate_admission(self, path):
        value = read_private(path)
        if set(value) != {'schema', 'marker', 'etag'} or value['schema'] != 1:
            raise StateError('Invalid durable admission')
        marker = value['marker']
        if (not isinstance(marker, dict) or set(marker) != {'schema', 'receipt', 'phase'}
                or marker['schema'] != 1 or marker['phase'] != 'admitted' or not isinstance(value['etag'], str)):
            raise StateError('Admission already consumed or corrupt')
        # Reuse the exact completion validator without trusting local marker data.
        receipt = self.directory / uuid.uuid4().hex
        original = dict(marker['receipt'])
        original['binding'] = hashlib.sha256(json.dumps(original, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        save_private(receipt, original)
        checked = completion(receipt, self.config)
        verify_bucket(self.config, checked['bucket_identity'])
        remote = self.directory / uuid.uuid4().hex
        response = self.call('get-object', '--key', self.admission_key(), '--if-match', value['etag'], str(remote))
        if response.get('ETag') != value['etag'] or remote.read_bytes() != json.dumps(marker, sort_keys=True).encode():
            raise StateError('Authoritative admission differs')
        return value

    def initial_check(self, admission, begin=False):
        value = self.validate_admission(admission)
        self.unlocked()
        # No current state, history, or unrelated objects may be reinterpreted.
        if self.inventory() != {self.admission_key()}:
            raise StateError('First deployment backend no longer empty; use ordinary encrypted state or explicit recovery')
        if begin:
            marker = dict(value['marker'], phase='applying')
            self.put(self.admission_key(), json.dumps(marker, sort_keys=True).encode(), '--if-match', value['etag'])
            # Persist local ambiguity BEFORE caller spawns OpenTofu. Failure here
            # also leaves remote applying fence: no repeated first apply.
            save_private(admission, dict(schema=1, marker=marker, etag=value['etag']), replace=True)
        return {'initial_operation': value['marker']['receipt']['operation']}

    def archive(self):
        self.unlocked()
        body, etag = self.get(self.key)
        key = self.prefix + hashlib.sha256(body).hexdigest() + '-' + uuid.uuid4().hex + '.tfstate'
        self.put(key, body, '--if-none-match', '*')
        self.unlocked()
        self.get(self.key, etag)
        return {'archive_key': key, 'source_etag': etag, 'sha256': hashlib.sha256(body).hexdigest()}

    def restore(self, key, archive_etag, current_etag):
        if not re.fullmatch(re.escape(self.prefix) + r'[a-f0-9]{64}-[a-f0-9]{32}\.tfstate', key):
            raise StateError('Archive belongs to a different state authority')
        body, _ = self.get(key, archive_etag)
        if hashlib.sha256(body).hexdigest() != key[len(self.prefix):][:64]:
            raise StateError('Archive content identity mismatch')
        self.unlocked()
        lock_id = uuid.uuid4().hex
        lock = json.dumps(dict(ID=lock_id, Operation='OperationTypeStatePush', Info='Explicit encrypted archive restore',
                               Who='bmux-recovery', Version='1.10.9', Path=self.key)).encode()
        lock_etag = self.put(self.key + '.tflock', lock, '--if-none-match', '*').get('ETag')
        if not lock_etag:
            raise StateError('Recovery lock identity unavailable; retain lock')
        # On any failure leave the recovery lock in place: ambiguous mutation must
        # be investigated, not unlocked automatically by a blind retry.
        self.get(self.key, current_etag)
        backup_body, _ = self.get(self.key, current_etag)
        backup_key = self.prefix + hashlib.sha256(backup_body).hexdigest() + '-' + uuid.uuid4().hex + '.tfstate'
        self.put(backup_key, backup_body, '--if-none-match', '*')
        self.put(self.key, body, '--if-match', current_etag)
        restored, _ = self.get(self.key)
        if restored != body:
            raise StateError('Restore outcome ambiguous; retain recovery lock')
        self.call('delete-object', '--key', self.key + '.tflock', '--if-match', lock_etag)
        return {'restored': key, 'previous_archive': backup_key}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('check', 'archive', 'list', 'restore', 'admit', 'begin'))
    parser.add_argument('--bootstrap-completion', type=Path)
    parser.add_argument('--admission-file', type=Path)
    parser.add_argument('--archive-key')
    parser.add_argument('--expected-archive-etag')
    parser.add_argument('--expected-current-etag')
    args = parser.parse_args()
    if args.action == 'restore' and not all((args.archive_key, args.expected_archive_etag, args.expected_current_etag)):
        parser.error('Restore requires exact archive key, archive ETag and current state ETag')
    if args.action == 'admit' and not all((args.bootstrap_completion, args.admission_file)):
        parser.error('Admission requires --bootstrap-completion and --admission-file')
    if args.action == 'begin' and not args.admission_file:
        parser.error('First apply requires --admission-file')
    os.umask(0o077)
    store = Store()
    try:
        if args.action == 'admit':
            result = store.admit(args.bootstrap_completion, args.admission_file)
        elif args.action == 'begin':
            result = store.initial_check(args.admission_file, begin=True)
        elif args.action in ('check', 'archive') and args.admission_file:
            result = store.initial_check(args.admission_file)
        elif args.action == 'check':
            store.unlocked()
            _, etag = store.get(store.key)
            result = {'state_etag': etag}
        elif args.action == 'archive':
            result = store.archive()
        elif args.action == 'list':
            values = store.call('list-objects-v2', '--prefix', store.prefix)
            if values.get('IsTruncated'):
                raise StateError('Truncated archive inventory rejected; use explicit key from protected operation record')
            result = {'archives': [{k: v[k] for k in ('Key', 'ETag', 'Size')} for v in values.get('Contents', [])]}
        else:
            result = store.restore(args.archive_key, args.expected_archive_etag, args.expected_current_etag)
        print(json.dumps(result))
    finally:
        store.temp.cleanup()


if __name__ == '__main__':
    try:
        main()
    except (StateError, OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired, urllib.error.URLError):
        print('R2 encrypted state operation rejected; check selected authority, encryption and exact ETags; provider output suppressed', file=sys.stderr)
        sys.exit(2)
