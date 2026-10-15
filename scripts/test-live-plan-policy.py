#!/usr/bin/env python3
"""Offline tests for infrastructure/release separation; no provider calls."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('policy', Path(__file__).with_name('live-plan-policy.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def plan(address, actions, **extra):
    return {'resource_changes': [{'address': address, 'change': {'actions': actions, **extra}}]}


class PlanPolicyTests(unittest.TestCase):
    def test_migration_stage_is_binding_free_and_excludes_route(self):
        after = {'bindings': [], 'migrations': {'new_tag': 'v1', 'new_sqlite_classes': ['LiveChannel']}}
        migration = plan('cloudflare_worker_version.live', ['create'], after=after)
        self.assertTrue(m.validate(migration, configure=True, migration=True))
        with self.assertRaises(ValueError):
            m.validate(migration, migration=True)
        for bindings in (None, [{'name': 'LIVE_CHANNELS'}]):
            with self.subTest(bindings=bindings), self.assertRaises(ValueError):
                m.validate(plan('cloudflare_worker_version.live', ['create'],
                                after=dict(after, bindings=bindings)), configure=True, migration=True)
        for address in ('cloudflare_workers_route.live', 'cloudflare_pages_project.live'):
            with self.subTest(address=address), self.assertRaises(ValueError):
                m.validate(plan(address, ['create']), configure=True, migration=True)

    def test_release_updates_version_and_deployment(self):
        for address in m.RELEASE_RESOURCES:
            self.assertTrue(m.validate(plan(address, ['update'])))
        self.assertTrue(m.validate(plan('cloudflare_worker_version.live', ['delete', 'create'])))
        self.assertTrue(m.validate(plan('cloudflare_workers_deployment.live', ['create', 'delete'])))
        with self.assertRaises(ValueError):
            m.validate(plan('cloudflare_workers_deployment.live', ['delete', 'create']), configure=True)
        self.assertTrue(m.validate({'resource_changes': []}))

    def test_infrastructure_drift_is_not_release(self):
        for address in ('cloudflare_pages_project.live', 'cloudflare_dns_record.live',
                        'cloudflare_worker.live', 'cloudflare_workers_route.live'):
            with self.assertRaises(ValueError):
                m.validate(plan(address, ['update']))
            self.assertTrue(m.validate(plan(address, ['update']), configure=True))
            self.assertTrue(m.validate(plan(address, ['no-op'])))

    def test_first_deploy_requires_configure(self):
        for address in ('cloudflare_pages_project.live', 'cloudflare_worker_version.live'):
            with self.assertRaises(ValueError):
                m.validate(plan(address, ['create']))
            self.assertTrue(m.validate(plan(address, ['create']), configure=True))

    def test_destructive_and_import_changes(self):
        for configure in (True, False):
            with self.assertRaises(ValueError):
                m.validate(plan('cloudflare_pages_project.live', ['delete', 'create']), configure)
        for actions in (['update'], ['no-op']):
            with self.assertRaises(ValueError):
                m.validate(plan('cloudflare_worker_version.live', actions, importing={'id': 'existing'}))

    def test_invalid_actions_rejected(self):
        for actions in ([], ['unknown'], None):
            with self.assertRaises(ValueError):
                m.validate(plan('cloudflare_worker_version.live', actions))

    def test_workflows_have_one_production_owner_and_shared_lock(self):
        root = Path(__file__).resolve().parents[1]
        release = (root / '.github/workflows/live-promote.yml').read_text()
        configure = (root / '.github/workflows/configure-production-infra.yml').read_text()
        self.assertNotIn('needs: [build, staging]', release)
        self.assertIn('environment: production-approval', release)
        self.assertIn('environment: Production', release)
        self.assertIn('group: bmux-production-infrastructure', release)
        self.assertIn('configure: true', configure)
        self.assertIn('uses: ./.github/workflows/live-promote.yml', configure)
        self.assertNotIn('id-token: write', release)
        self.assertNotIn('configure-aws-credentials', release)
        for name in ('R2_STATE_ACCESS_KEY_ID', 'R2_STATE_SECRET_ACCESS_KEY', 'TOFU_STATE_ENCRYPTION_PASSPHRASE'):
            self.assertIn(name, release)


if __name__ == '__main__':
    unittest.main()
