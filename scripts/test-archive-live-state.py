#!/usr/bin/env python3
"""Offline encrypted R2 history tests: all S3 operations are in-memory."""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('archive', Path(__file__).with_name('archive-live-state.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
BODY = json.dumps(dict(serial=1, lineage='offline-lineage', meta={'key': {}},
                       encrypted_data=base64.b64encode(b'encrypted-fixture-not-real-state').decode(),
                       encryption_version='v0')).encode()


class Memory(m.Store):
    def __init__(self):
        self.key = 'bmux/live/production/terraform.tfstate'
        self.prefix = 'history/' + self.key + '/'
        self.objects = {self.key: BODY}
        self.temp = tempfile.TemporaryDirectory()
        self.directory = Path(self.temp.name)
        self.calls = []
        self.conflict = False

    @staticmethod
    def etag(body):
        return '"' + hashlib.sha256(body).hexdigest() + '"'

    def call(self, operation, *args):
        self.calls.append((operation, args))
        def arg(name):
            return args[args.index(name) + 1] if name in args else None
        key = arg('--key')
        if operation == 'list-objects-v2':
            return {'Contents': [{'Key': k} for k in self.objects if k.startswith(arg('--prefix') or '')]}
        if operation in ('get-object', 'put-object', 'delete-object'):
            old = self.objects.get(key)
            if arg('--if-match') and (old is None or self.etag(old) != arg('--if-match')):
                raise m.StateError('Conditional identity conflict')
            if arg('--if-none-match') and old is not None:
                raise m.StateError('Already exists')
        if operation == 'get-object':
            if old is None:
                raise m.StateError('Missing authority')
            Path(args[-1]).write_bytes(old)
            return {'ETag': self.etag(old)}
        if operation == 'put-object':
            if self.conflict and key == self.key:
                raise m.StateError('Concurrent state writer changed ETag')
            self.objects[key] = Path(arg('--body')).read_bytes()
            return {'ETag': self.etag(self.objects[key])}
        if operation == 'delete-object':
            del self.objects[key]
            return {}
        raise AssertionError(operation)


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        self.store = Memory()
        self.addCleanup(self.store.temp.cleanup)

    def test_envelope_rejects_plaintext_corrupt_unknown(self):
        self.assertEqual(m.encrypted(BODY), BODY)
        for value in (b'{}', b'bad', BODY.replace(b'v0', b'v1'),
                      BODY.replace(b'"serial": 1', b'"resources": []'),
                      BODY.replace(b'ZW5jcnlwdGVk', b'!!')):
            with self.assertRaises(m.StateError):
                m.encrypted(value)

    def test_archive_is_encrypted_separate_unique_history(self):
        first = self.store.archive()
        second = self.store.archive()
        self.assertNotEqual(first['archive_key'], second['archive_key'])
        self.assertEqual(self.store.objects[first['archive_key']], BODY)
        self.assertEqual(self.store.objects[self.store.key], BODY)
        puts = [args for op, args in self.store.calls if op == 'put-object']
        self.assertTrue(all('--if-none-match' in args for args in puts))

    def test_missing_state_and_live_lock_fail_closed(self):
        self.store.objects.clear()
        with self.assertRaises(m.StateError):
            self.store.archive()
        self.store.objects[self.store.key] = BODY
        self.store.objects[self.store.key + '.tflock'] = b'lock'
        with self.assertRaises(m.StateError):
            self.store.archive()
        self.assertFalse(any(op == 'put-object' for op, _ in self.store.calls))

    def test_restore_requires_exact_two_identities_and_preserves_prior(self):
        archive = self.store.archive()['archive_key']
        self.store.objects[self.store.key] = BODY.replace(b'"serial": 1', b'"serial": 2')
        previous = self.store.objects[self.store.key]
        with self.assertRaises(m.StateError):
            self.store.restore(archive, '"wrong"', self.store.etag(previous))
        self.store.restore(archive, self.store.etag(BODY), self.store.etag(previous))
        self.assertEqual(self.store.objects[self.store.key], BODY)
        self.assertIn(previous, [v for k, v in self.store.objects.items() if k.startswith(self.store.prefix)])
        self.assertNotIn(self.store.key + '.tflock', self.store.objects)
        state_put = [args for op, args in self.store.calls if op == 'put-object' and self.store.key == args[1]][0]
        self.assertIn('--if-match', state_put)
        deletion = [args for op, args in self.store.calls if op == 'delete-object'][0]
        self.assertIn('--if-match', deletion)

    def test_restore_cas_failure_retains_recovery_lock(self):
        archive = self.store.archive()['archive_key']
        self.store.conflict = True
        with self.assertRaises(m.StateError):
            self.store.restore(archive, self.store.etag(BODY), self.store.etag(BODY))
        self.assertIn(self.store.key + '.tflock', self.store.objects)
        self.assertFalse(any(op == 'delete-object' for op, _ in self.store.calls))

    def test_restore_rejects_other_environment_and_lock(self):
        archive = self.store.archive()['archive_key']
        with self.assertRaises(m.StateError):
            self.store.restore(archive.replace('production', 'staging'), 'etag', 'etag')
        self.store.objects[self.store.key + '.tflock'] = b'other operator'
        with self.assertRaises(m.StateError):
            self.store.restore(archive, self.store.etag(BODY), self.store.etag(BODY))
        self.assertEqual(self.store.objects[self.store.key + '.tflock'], b'other operator')

    def test_bounded_private_process_timeout_and_output(self):
        with self.assertRaises(m.StateError):
            m.bounded_run([sys.executable, '-c', 'print("x"*10000)'], {}, limit=100)
        with self.assertRaises(m.StateError):
            m.bounded_run([sys.executable, '-c', 'import time; time.sleep(10)'], {}, timeout=0.05)
        result = m.bounded_run([sys.executable, '-c', 'print("ok")'], {})
        self.assertEqual(result.stdout, b'ok\n')

    def receipt(self):
        self.store.config = dict(bucket='new-state', region='auto', key=self.store.key,
                                 endpoint='https://' + 'a' * 32 + '.r2.cloudflarestorage.com', use_lockfile=True)
        value = dict(schema=1, kind='bmux-live-r2-bootstrap-completion', operation='d' * 32,
                     backend=dict(self.store.config, encrypt=True, account_id='a' * 32), created=True,
                     bucket_identity=dict(name='new-state', creation_date='2026-01-01', storage_class='Standard'))
        value['binding'] = hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        path = self.store.directory / 'completion.json'
        path.write_text(json.dumps(value))
        path.chmod(0o600)
        return path

    def test_admission_exact_receipt_new_generation_empty_and_one_shot(self):
        receipt = self.receipt()
        self.store.objects.clear()
        admission = self.store.directory / 'admission.json'
        with patch.dict(os.environ, CLOUDFLARE_ACCOUNT_ID='a' * 32), patch.object(m, 'verify_bucket'):
            self.store.admit(receipt, admission)
            self.assertNotIn(self.store.key, self.store.objects)  # no synthetic state
            self.assertIn(self.store.admission_key(), self.store.objects)
            self.store.initial_check(admission)
            resumed = self.store.directory / 'another.json'
            self.store.admit(receipt, resumed)
            self.store.initial_check(admission, begin=True)
            with self.assertRaises(m.StateError):
                self.store.initial_check(resumed, begin=True)
            with self.assertRaises(m.StateError):
                self.store.admit(receipt, self.store.directory / 'after-begin.json')
            with self.assertRaises(m.StateError):
                self.store.initial_check(admission, begin=True)
            with self.assertRaises(m.StateError):
                self.store.archive()  # ordinary missing-state never admitted
        self.assertEqual(m.read_private(admission)['marker']['phase'], 'applying')
        marker_puts = [args for op, args in self.store.calls if op == 'put-object' and args[1] == self.store.admission_key()]
        self.assertIn('--if-none-match', marker_puts[0])
        self.assertIn('--if-match', marker_puts[1])

    def test_resume_admission_is_read_only_and_requires_exact_marker_only_inventory(self):
        receipt = self.receipt()
        self.store.objects.clear()
        admission = self.store.directory / 'first.json'
        with patch.dict(os.environ, CLOUDFLARE_ACCOUNT_ID='a' * 32), patch.object(m, 'verify_bucket'):
            self.store.admit(receipt, admission)
            original = dict(self.store.objects)
            self.store.calls.clear()
            resumed = self.store.directory / 'resumed.json'
            self.store.admit(receipt, resumed)
            self.assertEqual(m.read_private(admission), m.read_private(resumed))
            self.assertEqual(self.store.objects, original)
            self.assertTrue(all(op in ('get-object', 'list-objects-v2') for op, _ in self.store.calls))
            # A validated but different operation cannot reuse the admission.
            value = json.loads(receipt.read_text())
            value['operation'] = 'e' * 32
            value.pop('binding')
            value['binding'] = hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
            other = self.store.directory / 'other-receipt.json'
            m.save_private(other, value)
            with self.assertRaises(m.StateError):
                self.store.admit(other, self.store.directory / 'wrong.json')
            for key in (self.store.key, self.store.key + '.tflock', self.store.prefix + 'archive', 'unrelated'):
                with self.subTest(key=key):
                    self.store.objects = dict(original, **{key: BODY})
                    with self.assertRaises(m.StateError):
                        self.store.admit(receipt, self.store.directory / 'blocked.json')
            self.store.objects = dict(original)
            with patch.object(m, 'verify_bucket', side_effect=m.StateError('Generation changed')):
                with self.assertRaises(m.StateError):
                    self.store.admit(receipt, self.store.directory / 'replaced.json')
            self.store.objects[self.store.admission_key()] = b'corrupt'
            with self.assertRaises(m.StateError):
                self.store.admit(receipt, self.store.directory / 'corrupt.json')

    def test_admission_rejects_existing_adopted_wrong_generation(self):
        receipt = self.receipt()
        admission = self.store.directory / 'admission.json'
        with patch.dict(os.environ, CLOUDFLARE_ACCOUNT_ID='a' * 32):
            with patch.object(m, 'verify_bucket', side_effect=m.StateError('Generation changed')):
                with self.assertRaises(m.StateError):
                    self.store.admit(receipt, admission)
            with patch.object(m, 'verify_bucket'):
                with self.assertRaises(m.StateError):
                    self.store.admit(receipt, admission)
            value = json.loads(receipt.read_text())
            value['created'] = False
            value.pop('binding')
            value['binding'] = hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
            receipt.write_text(json.dumps(value))
            with self.assertRaises(m.StateError):
                m.completion(receipt, self.store.config)
        self.assertFalse(any(op == 'put-object' for op, _ in self.store.calls))

    def test_admission_ambiguous_marker_ack_cannot_reconsume(self):
        receipt = self.receipt()
        self.store.objects.clear()
        admission = self.store.directory / 'admission.json'
        with patch.dict(os.environ, CLOUDFLARE_ACCOUNT_ID='a' * 32), patch.object(m, 'verify_bucket'), \
                patch.object(m, 'save_private', side_effect=OSError('Interrupted')):
            with self.assertRaises(OSError):
                self.store.admit(receipt, admission)
        self.assertIn(self.store.admission_key(), self.store.objects)
        self.assertIn(self.store.key + '.tflock', self.store.objects)
        self.assertNotIn(self.store.key, self.store.objects)

    def test_provider_free_real_tofu_encryption(self):
        import shutil
        tofu = shutil.which('tofu')
        if not tofu:
            self.skipTest('OpenTofu unavailable for provider-free fixture')
        with tempfile.TemporaryDirectory(prefix='bmux-encryption-fixture-') as directory:
            root = Path(directory)
            root.joinpath('main.tf').write_text((Path(__file__).parent.parent / 'infra/live-terraform/encryption.tf').read_text()
                                              + '\noutput "fixture" { value = "fixture-plaintext" }\n')
            env = {k: v for k, v in os.environ.items() if not k.startswith(('TF_', 'TOFU_', 'AWS_'))}
            env['TF_VAR_state_encryption_passphrase'] = 'offline-only-fixture-passphrase-not-a-reusable-secret'
            for args in (('init', '-backend=false'), ('plan', '-out=plan'), ('apply', '-auto-approve', 'plan')):
                result = m.bounded_run([tofu, *args], env, cwd=root)
                self.assertEqual(result.returncode, 0, 'Provider-free local encryption fixture failed')
            body = root.joinpath('terraform.tfstate').read_bytes()
            m.encrypted(body)
            self.assertNotIn(b'fixture-plaintext', body)
            self.assertNotIn(b'fixture-plaintext', root.joinpath('plan').read_bytes())


class TransportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.store = object.__new__(m.Store)
        self.store.config = dict(bucket='bmux-state', endpoint='https://' + 'a' * 32 + '.r2.cloudflarestorage.com')
        self.store.access_key = 'offline-access'
        self.store.secret_key = 'offline-secret'
        self.store.directory = Path(self.temp.name)

    def connection(self, body=b'', status=200, etag='"etag"'):
        import io
        from unittest.mock import Mock
        response = Mock()
        response.status = status
        response.read.side_effect = io.BytesIO(body).read
        response.getheader.side_effect = lambda name: etag if name == 'ETag' else None
        connection = Mock()
        connection.getresponse.return_value = response
        return connection

    def test_signed_https_preserves_conditions_and_encoding(self):
        connection = self.connection(BODY)
        with patch.object(m.http.client, 'HTTPSConnection', return_value=connection) as create:
            data, etag = self.store.request('PUT', 'a b/+%ü', body=BODY, conditions={'if-none-match': '*'})
        create.assert_called_once_with('a' * 32 + '.r2.cloudflarestorage.com', timeout=30)
        args, kwargs = connection.request.call_args
        self.assertEqual(args, ('PUT', '/bmux-state/a%20b/%2B%25%C3%BC'))
        headers = kwargs['headers']
        self.assertEqual(headers['if-none-match'], '*')
        self.assertEqual(headers['x-amz-content-sha256'], hashlib.sha256(BODY).hexdigest())
        self.assertIn('/auto/s3/aws4_request', headers['authorization'])
        self.assertIn('SignedHeaders=host;if-none-match;x-amz-content-sha256;x-amz-date', headers['authorization'])
        self.assertNotIn('offline-secret', str(connection.request.call_args))
        self.assertEqual((data, etag), (BODY, '"etag"'))
        connection.close.assert_called_once()

    def test_sigv4_matches_independent_s3_signer_fixture(self):
        from datetime import datetime, timezone
        connection = self.connection()
        with patch.object(m, 'datetime') as date, patch.object(m.http.client, 'HTTPSConnection', return_value=connection):
            date.now.return_value = datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
            self.store.request('PUT', 'a b/+%ü', body=b'offline', conditions={'if-none-match': '*'})
        # Generated independently with botocore.auth.S3SigV4Auth, not this implementation.
        signature = connection.request.call_args.kwargs['headers']['authorization'].split('Signature=')[1]
        self.assertEqual(signature, '05a0538fc66a197fe2d014142a91c114c09b84da4d7c352b3ea8069cd3b8b035')

    def test_truncated_and_oversized_content_length_rejects(self):
        for length in ('10', str(33 * 1024 * 1024), 'invalid'):
            connection = self.connection(b'abc')
            connection.getresponse.return_value.getheader.side_effect = lambda name: length if name == 'Content-Length' else '"e"'
            with patch.object(m.http.client, 'HTTPSConnection', return_value=connection), self.assertRaises(m.StateError):
                self.store.request('GET', 'state')
            connection.close.assert_called_once()

    def test_errors_and_redirects_never_retry_or_leak(self):
        for status in (301, 403, 404, 412, 500):
            connection = self.connection(b'SECRET_PROVIDER_RESPONSE', status=status)
            with patch.object(m.http.client, 'HTTPSConnection', return_value=connection):
                with self.assertRaises(m.StateError) as error:
                    self.store.request('DELETE', 'state', conditions={'if-match': '"expected"'})
            self.assertNotIn('SECRET_PROVIDER_RESPONSE', str(error.exception))
            connection.request.assert_called_once()
            connection.close.assert_called_once()
        self.store.config['endpoint'] = 'https://attacker.invalid'
        with patch.object(m.http.client, 'HTTPSConnection') as create, self.assertRaises(m.StateError):
            self.store.request('GET')
        create.assert_not_called()

    def test_response_bounds_and_deadline_close_without_retry(self):
        from unittest.mock import Mock
        connection = self.connection()
        connection.getresponse.return_value.read.side_effect = lambda size: b'x' * size
        with patch.object(m.http.client, 'HTTPSConnection', return_value=connection), self.assertRaises(m.StateError):
            self.store.request('GET', 'state')
        connection.close.assert_called_once()
        connection = self.connection()
        with patch.object(m.http.client, 'HTTPSConnection', return_value=connection), \
                patch.object(m.time, 'monotonic', side_effect=[0, 121]), self.assertRaises(m.StateError):
            self.store.request('GET', 'state')
        connection.close.assert_called_once()
        connection = self.connection()
        connection.request.side_effect = TimeoutError('SECRET_TRANSPORT')
        with patch.object(m.http.client, 'HTTPSConnection', return_value=connection):
            with self.assertRaises(m.StateError) as error:
                self.store.request('PUT', 'state', body=BODY)
        self.assertNotIn('SECRET_TRANSPORT', str(error.exception))
        connection.request.assert_called_once()
        connection.close.assert_called_once()

    def test_object_adapter_preserves_cas_and_requires_acknowledgement(self):
        body = self.store.directory / 'body'
        body.write_bytes(BODY)
        with patch.object(self.store, 'request', return_value=(b'', '"committed"')) as request:
            self.assertEqual(self.store.call('put-object', '--key', 'state', '--body', str(body),
                                             '--if-match', '"original"'), {'ETag': '"committed"'})
            self.assertEqual(request.call_args.kwargs['conditions'], {'if-match': '"original"'})
        with patch.object(self.store, 'request', return_value=(b'', None)), self.assertRaises(m.StateError):
            self.store.call('put-object', '--key', 'state', '--body', str(body), '--if-none-match', '*')
        output = self.store.directory / 'download'
        with patch.object(self.store, 'request', return_value=(BODY, '"expected"')):
            self.store.call('get-object', '--key', 'state', '--if-match', '"expected"', str(output))
        self.assertEqual(output.read_bytes(), BODY)

    def test_xml_inventory_unknown_corrupt_and_truncated(self):
        xml = (b'<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
               b'<Name>bmux-state</Name><IsTruncated>true</IsTruncated><Contents>'
               b'<Key>state</Key><ETag>&quot;e&quot;</ETag><Size>12</Size></Contents></ListBucketResult>')
        with patch.object(self.store, 'request', return_value=(xml, None)):
            result = self.store.call('list-objects-v2', '--prefix', 'history/')
        self.assertTrue(result['IsTruncated'])
        self.assertEqual(result['Contents'], [dict(Key='state', ETag='"e"', Size=12)])
        for bad in (b'{broken', xml.replace(b'bmux-state', b'foreign'), xml.replace(b'true', b'unknown'),
                    b'<!DOCTYPE x [<!ENTITY y "boom">]>' + xml):
            with patch.object(self.store, 'request', return_value=(bad, None)), self.assertRaises(m.StateError):
                self.store.call('list-objects-v2')


if __name__ == '__main__':
    unittest.main()
