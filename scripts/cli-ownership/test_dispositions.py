"""Regression checks for disabled-plugin diagnostics and the retained-path ledger."""
import unittest
from unittest.mock import patch

import compat
from dispositions import disposition, record


class DisabledDiagnostics(unittest.TestCase):
    def test_first_missing_segment_and_surviving_parent(self):
        old = record(0, 'original help\n')
        for case, word, usage in [
            ('help remote trust remove', 'remote', 'bmux [OPTIONS] [COMMAND]'),
            ('logs profiles list --json', 'profiles', 'bmux logs [OPTIONS] <COMMAND>'),
            ('help server save', 'save', 'bmux server [OPTIONS] <COMMAND>'),
        ]:
            with self.subTest(case=case):
                error = f"error: unrecognized subcommand '{word}'\n\nUsage: {usage}\n\nFor more information, try '--help'.\n"
                self.assertEqual(disposition(case, 'disabled', old, record(2, stderr=error)), 'owner absent')
                for wrong in [record(1, stderr=error), record(2, 'leak', error), old,
                              record(2, stderr=error.replace(word, 'other', 1)),
                              record(2, stderr=error.replace(usage, 'bmux OTHER'))]:
                    self.assertEqual(disposition(case, 'disabled', old, wrong), 'violation')
        self.assertIsNone(disposition('help remote invented', 'disabled', old, record(2)))
        self.assertIsNone(disposition('help logs path', 'disabled', old, record(2)))

    def test_catalog_covers_non_migrated_owners(self):
        compat.check_catalog()
        with patch.dict(compat.AUDIT['theme show'], {'owner': 'bmux.other'}):
            with self.assertRaisesRegex(ValueError, 'theme show'):
                compat.check_catalog()
