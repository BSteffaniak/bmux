#!/usr/bin/env python3
"""Encrypted R2 history/recovery. S3 protocol only; no AWS account operations.

Restore uses conditional PUT and a temporary OpenTofu-compatible lock. The lock
cooperates with OpenTofu; it is not a transaction with unrelated S3 writers.

Completed cutover markers remain durable replay fences. Release checks read them
but never reset or mutate them. Archive/restore retain canonical encrypted state.
"""
import argparse
import base64
import hashlib
import hmac
from datetime import datetime, timezone
import http.client
import xml.etree.ElementTree as ET
import urllib.parse
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


class MigrationError(StateError):
    """Fixed, secret-free status; never include provider or marker contents."""
    def __init__(self, status):
        self.status = status
        super().__init__(status)


def migration_hash(value):
    return isinstance(value, str) and re.fullmatch(r'[a-f0-9]{64}', value) is not None


def migration_etag(value):
    return isinstance(value, str) and re.fullmatch(r'"(?:[a-f0-9]{32}|[a-f0-9]{64})"', value) is not None


def migration_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise MigrationError('migration_record_invalid')
        result[key] = value
    return result


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


def read_private(path, object_pairs_hook=None):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 1024 * 1024:
            raise StateError('Unsafe durable admission file')
        return json.load(stream, object_pairs_hook=object_pairs_hook)


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
        self.access_key = os.environ['R2_STATE_ACCESS_KEY_ID']
        self.secret_key = os.environ['R2_STATE_SECRET_ACCESS_KEY']
        self.temp = tempfile.TemporaryDirectory(prefix='bmux-state-')
        self.directory = Path(self.temp.name)

    def request(self, method, key='', query=None, body=b'', conditions=None):
        """One bounded SigV4 HTTPS request to the selected R2 authority; never retry writes."""
        endpoint = urllib.parse.urlsplit(self.config['endpoint'])
        if (endpoint.scheme != 'https' or endpoint.netloc != endpoint.hostname
                or not re.fullmatch(r'[a-f0-9]{32}\.r2\.cloudflarestorage\.com', endpoint.hostname or '')
                or endpoint.path or endpoint.query or endpoint.fragment):
            raise StateError('Noncanonical R2 endpoint rejected')
        if len(body) > 32 * 1024 * 1024:
            raise StateError('R2 object exceeds bound')
        path = '/' + urllib.parse.quote(self.config['bucket'], safe='')
        if key:
            path += '/' + urllib.parse.quote(key, safe='/~')
        encoded_query = '&'.join(sorted(urllib.parse.quote(k, safe='~') + '=' + urllib.parse.quote(str(v), safe='~')
                                        for k, v in (query or {}).items()))
        now = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')
        payload_hash = hashlib.sha256(body).hexdigest()
        headers = {'host': endpoint.hostname, 'x-amz-date': now, 'x-amz-content-sha256': payload_hash}
        headers.update(conditions or {})
        names = ';'.join(sorted(headers))
        canonical_headers = ''.join(k + ':' + headers[k].strip() + '\n' for k in sorted(headers))
        canonical = '\n'.join((method, path, encoded_query, canonical_headers, names, payload_hash))
        scope = now[:8] + '/auto/s3/aws4_request'
        to_sign = 'AWS4-HMAC-SHA256\n' + now + '\n' + scope + '\n' + hashlib.sha256(canonical.encode()).hexdigest()
        signing_key = ('AWS4' + self.secret_key).encode()
        for part in (now[:8], 'auto', 's3', 'aws4_request'):
            signing_key = hmac.new(signing_key, part.encode(), hashlib.sha256).digest()
        signature = hmac.new(signing_key, to_sign.encode(), hashlib.sha256).hexdigest()
        headers['authorization'] = ('AWS4-HMAC-SHA256 Credential=' + self.access_key + '/' + scope
                                    + ', SignedHeaders=' + names + ', Signature=' + signature)
        connection = http.client.HTTPSConnection(endpoint.hostname, timeout=30)
        deadline = time.monotonic() + 120
        try:
            connection.request(method, path + ('?' + encoded_query if encoded_query else ''), body=body, headers=headers)
            response = connection.getresponse()
            if response.status not in (200, 204):
                raise StateError('R2 request rejected; reconcile exact identities before retry; response suppressed')
            length = response.getheader('Content-Length')
            if length is not None and (not length.isdigit() or int(length) > 32 * 1024 * 1024):
                raise StateError('Invalid or oversized R2 response length')
            blocks = bytearray()
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise StateError('R2 deadline exceeded; mutation may be ambiguous')
                if connection.sock:
                    connection.sock.settimeout(min(30, remaining))
                block = response.read(min(65536, 32 * 1024 * 1024 + 1 - len(blocks)))
                blocks.extend(block)
                if len(blocks) > 32 * 1024 * 1024:
                    raise StateError('R2 response exceeds bound')
                if not block:
                    break
            if length is not None and len(blocks) != int(length):
                raise StateError('Truncated R2 response rejected')
            return bytes(blocks), response.getheader('ETag')
        except (OSError, http.client.HTTPException, ValueError):
            raise StateError('R2 transport failed; mutation may be ambiguous; response suppressed') from None
        finally:
            connection.close()

    def call(self, operation, *args):
        # Keep the private storage adapter contract shared by recovery/admission.
        options = {}
        destination = None
        i = 0
        while i < len(args):
            if args[i].startswith('--') and i + 1 < len(args):
                options[args[i]] = args[i + 1]
                i += 2
            elif operation == 'get-object' and i == len(args) - 1:
                destination = Path(args[i])
                i += 1
            else:
                raise StateError('Unsupported R2 operation arguments')
        conditions = {name[2:]: options[name] for name in ('--if-match', '--if-none-match') if name in options}
        if operation == 'list-objects-v2':
            body, _ = self.request('GET', query={'list-type': '2', 'prefix': options.get('--prefix', ''),
                                                'max-keys': options.get('--max-keys', '1000')})
            try:
                if b'<!DOCTYPE' in body.upper() or b'<!ENTITY' in body.upper():
                    raise ValueError('Unsupported XML declarations')
                root = ET.fromstring(body)
                ns = '{http://s3.amazonaws.com/doc/2006-03-01/}'
                if root.tag != ns + 'ListBucketResult' or root.findtext(ns + 'IsTruncated') not in ('true', 'false'):
                    raise ValueError('Unsupported listing')
                if root.findtext(ns + 'Name') != self.config['bucket']:
                    raise ValueError('Wrong bucket')
                contents = []
                for item in root.findall(ns + 'Contents'):
                    key, etag, size = (item.findtext(ns + name) for name in ('Key', 'ETag', 'Size'))
                    if not key or not etag or size is None or int(size) < 0:
                        raise ValueError('Corrupt listing')
                    contents.append(dict(Key=key, ETag=etag, Size=int(size)))
                return dict(Contents=contents, KeyCount=len(contents),
                            IsTruncated=root.findtext(ns + 'IsTruncated') == 'true')
            except (ET.ParseError, ValueError):
                raise StateError('Corrupt R2 inventory rejected') from None
        if operation not in ('get-object', 'put-object', 'delete-object') or '--key' not in options:
            raise StateError('Unsupported R2 operation')
        body = Path(options['--body']).read_bytes() if operation == 'put-object' else b''
        data, etag = self.request({'get-object': 'GET', 'put-object': 'PUT', 'delete-object': 'DELETE'}[operation],
                                  options['--key'], body=body, conditions=conditions)
        if operation != 'delete-object' and not etag:
            raise StateError('R2 object acknowledgement lacks identity; mutation may be ambiguous')
        if operation == 'get-object':
            if destination is None:
                raise StateError('Missing private object destination')
            with destination.open('xb') as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
        return {'ETag': etag}

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
        inventory = self.inventory()
        if inventory == {self.admission_key()}:
            # Resume only the same pre-apply admission, never infer a new lineage
            # from missing state. No remote write or lock is replayed here; begin
            # still fences the single apply using the exact authoritative ETag.
            remote = self.directory / uuid.uuid4().hex
            response = self.call('get-object', '--key', self.admission_key(), str(remote))
            marker = dict(schema=1, receipt=value, phase='admitted')
            etag = response.get('ETag')
            if not etag or remote.read_bytes() != json.dumps(marker, sort_keys=True).encode():
                raise StateError('Bootstrap receipt differs or apply already started; explicit recovery required')
            save_private(output, dict(schema=1, marker=marker, etag=etag))
            self.initial_check(output)
            return {'admitted_operation': value['operation']}
        if inventory:
            raise StateError('First deployment requires empty bucket or exact pre-apply admission')
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
        # Persist the acknowledgement after the remote marker. A fresh runner may
        # recover this acknowledgement only while the exact marker is admitted
        # and remains the sole object; an applying fence is never replayed.
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

    def migration_key(self):
        return 'migration/' + self.key + '.json'

    def migration_marker(self):
        key = self.migration_key()
        values = self.call('list-objects-v2', '--prefix', key, '--max-keys', '2')
        rows = values.get('Contents', [])
        if values.get('IsTruncated') or not isinstance(rows, list):
            raise MigrationError('migration_marker_invalid')
        if not rows:
            return None
        if len(rows) != 1 or rows[0].get('Key') != key:
            raise MigrationError('migration_marker_invalid')
        path = self.directory / uuid.uuid4().hex
        response = self.call('get-object', '--key', key, str(path))
        etag = response.get('ETag')
        raw = path.read_bytes()
        if not migration_etag(etag) or len(raw) > 4096:
            raise MigrationError('migration_marker_invalid')
        marker = json.loads(raw, object_pairs_hook=migration_object)
        base = {'schema', 'operation', 'migration_digest', 'source_sha256', 'phase'}
        if (not isinstance(marker, dict) or type(marker.get('schema')) is not int or marker['schema'] != 1
                or marker.get('phase') not in ('applying', 'completed')
                or set(marker) != (base | {'applying_etag'} if marker.get('phase') == 'completed' else base)
                or not migration_hash(marker.get('migration_digest')) or not migration_hash(marker.get('source_sha256'))
                or not isinstance(marker.get('operation'), str)
                or str(uuid.UUID(marker['operation'])) != marker['operation']
                or (marker['phase'] == 'completed' and not migration_etag(marker['applying_etag']))):
            raise MigrationError('migration_marker_invalid')
        return marker, etag

    def release_check(self):
        self.unlocked()
        body, etag = self.get(self.key)
        marker = self.migration_marker()
        if marker and marker[0]['phase'] != 'completed':
            raise StateError('Incomplete cutover requires reviewed state recovery')
        self.unlocked()
        self.get(self.key, etag)
        if self.migration_marker() != marker:
            raise StateError('Cutover fence changed during inspection')
        return {'state_etag': etag, 'sha256': hashlib.sha256(body).hexdigest()}

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
    parser.add_argument('action', choices=('check', 'archive', 'list', 'restore', 'admit', 'begin',
                                         'release-check'))
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
        if args.action == 'release-check':
            result = store.release_check()
        elif args.action == 'admit':
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
    except (StateError, OSError, ValueError, KeyError, TypeError, AttributeError, subprocess.TimeoutExpired, urllib.error.URLError) as error:
        print('R2 encrypted state operation rejected; check selected authority, encryption and exact ETags; provider output suppressed', file=sys.stderr)
        sys.exit(2)
