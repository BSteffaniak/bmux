#!/usr/bin/env python3
"""Offline policy regressions for the Fly-only end state."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('policy', Path(__file__).with_name('live-plan-policy.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def plan(address, actions, **values):
    return {'resource_changes': [{'address': address, 'change': {'actions': actions, **values}}]}


class PolicyTests(unittest.TestCase):
    def test_image_only_update(self):
        before = {'image': 'old', 'region': 'iad', 'env': {'key': 'digest'}}
        after = dict(before, image='new')
        self.assertTrue(m.validate(plan('fly_machine.live', ['update'], before=before, after=after)))
        after['region'] = 'lax'
        with self.assertRaises(ValueError): m.validate(plan('fly_machine.live', ['update'], before=before, after=after))

    def test_configure_only_non_destructive_known_resources(self):
        for address in m.CONFIGURE_RESOURCES:
            self.assertTrue(m.validate(plan(address, ['create']), configure=True))
            with self.assertRaises(ValueError): m.validate(plan(address, ['delete']), configure=True)
        with self.assertRaises(ValueError): m.validate(plan('other.resource', ['create']), configure=True)

    def test_legacy_worker_mutations_are_never_admitted(self):
        for address in ('cloudflare_worker.live', 'cloudflare_worker_version.live',
                        'cloudflare_workers_deployment.live', 'cloudflare_workers_route.live'):
            for actions in (['no-op'], ['create'], ['update'], ['delete']):
                for configure in (False, True):
                    with self.assertRaises(ValueError):
                        m.validate(plan(address, actions), configure=configure)

    def test_unknown_plan_and_imports_fail_closed(self):
        for extra in ({'errored': True}, {'complete': False}, {'deferred_changes': [{}]}):
            with self.assertRaises(ValueError):
                m.validate(dict(resource_changes=[], **extra))
        candidate = plan('fly_machine.live', ['no-op'], importing={'id': 'unexpected'})
        with self.assertRaises(ValueError):
            m.validate(candidate)
        self.assertTrue(m.validate(candidate, configure=True))

    def test_no_release_create_or_replacement(self):
        for actions in [['create'], ['delete'], ['delete', 'create'], ['create', 'delete']]:
            with self.assertRaises(ValueError): m.validate(plan('fly_machine.live', actions))


if __name__ == '__main__': unittest.main()
