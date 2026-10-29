#!/usr/bin/env python3
"""Regression tests for bounded, credential-safe deployment errors."""
import io
import importlib.util
import json
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('diagnostics', Path(__file__).with_name('live-deploy-diagnostics.py'))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DiagnosticsTests(unittest.TestCase):
    def fly_record(self, body):
        return json.dumps({'diagnostic': {'severity': 'error', 'summary': 'Update Machine Error',
            'detail': 'fly machines: 400 Bad Request (request_id: 01M64CCD7TK48GC2W92MYW6VVS-ord, body: '
                      + json.dumps(body) + ')'}})

    def test_fly_error_is_actionable_without_printing_echoed_configuration(self):
        for message, expected in [
            ('invalid image: manifest not found', 'image rejected'),
            ('volume is attached to another host', 'volume placement rejected'),
            ('machine lease has expired', 'lease conflict'),
            ('memory_mb must be at least 256', 'guest sizing rejected'),
            ('service internal_port is invalid', 'service configuration rejected'),
            ('restart policy is invalid', 'restart policy rejected'),
            ('not enough capacity on host', 'capacity unavailable'),
            ('machine not found', 'machine state conflict'),
            ('region is unsupported', 'region rejected'),
        ]:
            with self.subTest(message=message):
                body = {'errors': [{'message': message + ' "unknown-private-value"'}],
                        'config': {'env': {'TOKEN': 'unlisted-private-token'}},
                        'token': 'private-token', 'metadata': 'private-metadata'}
                result = MODULE.diagnostics(self.fly_record(body), {'FLY_API_TOKEN': 'private-token'})
                self.assertIn(expected, result)
                self.assertIn('classification (heuristic)', result)
                self.assertIn('01M64CCD7TK48GC2W92MYW6VVS-ord', result)
                for private in ('unknown-private-value', 'unlisted-private-token', 'private-token', 'private-metadata'):
                    self.assertNotIn(private, result)

    def test_fly_unknown_error_remains_private_and_does_not_guess_from_config(self):
        record = self.fly_record({'error': 'undocumented "private-remote-text"',
                                  'config': {'error': 'invalid image manifest'}})
        result = MODULE.diagnostics(record, {})
        self.assertIn('unclassified rejection', result)
        self.assertNotIn('private-remote-text', result)
        self.assertNotIn('image rejected', result)

    def test_secret_value_cannot_create_a_spurious_fly_classification(self):
        result = MODULE.diagnostics(self.fly_record({'error': 'volume attached'}),
                                    {'FLY_API_TOKEN': 'volume attached'})
        self.assertIn('unclassified rejection', result)
        self.assertNotIn('volume placement rejected', result)

    def test_encoded_control_text_is_never_printed_by_classifier(self):
        result = MODULE.diagnostics(self.fly_record({'error': 'invalid image\n::error::private\u001b[31m'}), {})
        self.assertIn('image rejected', result)
        self.assertNotIn('::error::private', result)
        self.assertNotIn('\\u001b', result)

    def test_malformed_fly_body_fails_closed(self):
        record = {'diagnostic': {'severity': 'error', 'summary': 'Update Machine Error',
                  'detail': 'fly machines: 400 Bad Request (request_id: request-123, body: {"error":"unterminated)'}}
        result = MODULE.diagnostics(json.dumps(record), {})
        self.assertIn('malformed', result)
        self.assertNotIn('unterminated', result)
        self.assertIn('request-123', result)

    def test_fly_integration_shape_does_not_expose_multiline_body_or_trailer(self):
        record = {'diagnostic': {'severity': 'error', 'summary': 'Update Machine Error',
                  'detail': 'fly machines: 400 Bad Request (request_id: request-123, body: '
                            '{\n"error":"volume is attached to another host",'
                            '"env":{"TOKEN":"private-config"}})\nprivate-trailer'}}
        result = MODULE.diagnostics(json.dumps(record), {})
        self.assertIn('volume placement rejected', result)
        self.assertNotIn('private-config', result)
        self.assertNotIn('private-trailer', result)

    def test_large_apply_log_keeps_late_errors_and_skips_oversized_records(self):
        text = ('{"type":"apply_progress"}\n' * 50000
                + 'x' * 70000 + '\n' + self.fly_record({'error': 'invalid image'}) + '\n')
        result = MODULE.diagnostic_lines(MODULE.bounded_lines(io.StringIO(text)), {})
        self.assertIn('image rejected', result)
        self.assertLess(len(result), 4000)

    def test_structured_errors_are_useful_and_redacted(self):
        record = {'diagnostic': {'severity': 'error', 'summary': 'Failed to read resource',
                  'detail': 'HTTP 403 token=private-token https://example.test/private "private-value"'}}
        result = MODULE.diagnostics(json.dumps(record), {'FLY_API_TOKEN': 'private-token'})
        self.assertIn('Failed to read resource', result)
        self.assertIn('HTTP 403', result)
        for private in ('private-token', 'private-value', 'example.test'):
            self.assertNotIn(private, result)

    def test_missing_schema_argument_name_is_visible(self):
        record = {'diagnostic': {'severity': 'error', 'summary': 'Missing required argument',
                  'detail': 'The argument "org_slug" is required, but no definition was found.'}}
        result = MODULE.diagnostics(json.dumps(record), {})
        self.assertIn('The argument org_slug is required', result)

    def test_raw_logs_and_non_errors_are_never_printed(self):
        result = MODULE.diagnostics('raw private credentials\n' + json.dumps(
            {'diagnostic': {'severity': 'warning', 'detail': 'private warning'}}), {})
        self.assertNotIn('credentials', result)
        self.assertNotIn('private warning', result)

    def test_output_is_bounded_and_workflow_commands_cannot_execute(self):
        record = json.dumps({'diagnostic': {'severity': 'error', 'summary': '::error::injected',
                            'detail': 'x' * 10000}})
        result = MODULE.diagnostics('\n'.join([record] * 100), {})
        self.assertLess(len(result), 16000)
        self.assertTrue(all(line.startswith('  ') for line in result.splitlines()))


if __name__ == '__main__':
    unittest.main()
