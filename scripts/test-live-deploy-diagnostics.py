#!/usr/bin/env python3
"""Regression tests for bounded, credential-safe deployment errors."""
import importlib.util
import json
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location('diagnostics', Path(__file__).with_name('live-deploy-diagnostics.py'))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DiagnosticsTests(unittest.TestCase):
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
