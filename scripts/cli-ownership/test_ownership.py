import json
import unittest
from unittest.mock import patch

import dispositions as policy
import ownership


class OwnershipTests(unittest.TestCase):
    def test_regressions_in_registration_and_exposure(self):
        old = json.loads(ownership.FIXTURE.read_text())
        current = ownership.inventory()
        original = ownership.Path.read_text

        def drop(path, *args, **kwargs):
            text = original(path, *args, **kwargs)
            if path.as_posix().endswith('connections-plugin/plugin.toml'):
                blocks = text.split('[[commands]]')
                for i, block in enumerate(blocks):
                    if 'name = "access-init"' in block:
                        blocks[i] = block.replace('expose_in_cli = true', 'expose_in_cli = false')
                return '[[commands]]'.join(blocks)
            if path.as_posix().endswith('connections-plugin/src/lib.rs'):
                return text.replace('.blocking_command("remote-upgrade",', '.blocking_command("wrong-name",')
            return text

        with patch.object(ownership.Path, 'read_text', drop):
            failures = ownership.migrated_errors(old, current)
        self.assertEqual({f.split(':')[0] for f in failures}, {'access init', 'remote upgrade'})

    def test_retained_path_cannot_silently_switch_plugins(self):
        old = json.loads(ownership.FIXTURE.read_text())
        current = ownership.inventory()
        current['paths']['theme show']['declarations'] = ['bmux.other:show']
        self.assertIn('theme show: changed plugin owner or callback without a migration disposition',
                      ownership.migrated_errors(old, current))

    def test_no_failure_masking(self):
        good = policy.record(0, 'original help\n')
        absent = policy.record(2, stderr='error: unrecognized subcommand\n')
        self.assertEqual(policy.disposition('help share', 'bare', good, absent), 'owner absent')
        self.assertEqual(policy.disposition('help share', 'bare', good, good), 'violation')
        self.assertEqual(policy.disposition('help share', 'disabled', good, policy.record(101)), 'violation')
        self.assertIsNone(policy.disposition('help share', 'default', good, absent))
        self.assertIsNone(policy.disposition('help session clients', 'local-splits', good, absent))
        self.assertIsNone(policy.disposition('help attach', 'bare', good, absent))

    def test_projection_keeps_order_description_and_siblings(self):
        def help_text(s):
            return policy.record(0, 'Usage: bmux logs\n\nCommands:\n' + s + '\n\nOptions:\n  --help\n')
        before = help_text('  tail    Tail logs\n  path    Log path\n  level   Log level')
        after = help_text('  path  Log path\n  level  Log level')
        self.assertEqual(policy.disposition('help logs', 'bare', before, after), 'namespace lists remaining children')
        self.assertIsNone(policy.disposition('help logs', 'bare', before, help_text('  level  Log level')))
        self.assertIsNone(policy.disposition('help logs', 'bare', before, help_text('  level  Log level\n  path  Log path')))
