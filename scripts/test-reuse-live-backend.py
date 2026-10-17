#!/usr/bin/env python3
"""Pinned backend reuse: integrity, provenance, and current infrastructure."""
import hashlib
import importlib.util
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('reuse', Path(__file__).with_name('reuse-live-backend.py'))
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ReuseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        for name in ('native/bmux', 'relay/image.oci.tar', 'relay/IMAGE', 'tools/package.json'):
            path = self.source / name
            path.parent.mkdir(exist_ok=True)
            path.write_text('immutable payload')
        (self.source / 'REVISION').write_text(MODULE.SOURCE_REVISION)
        inventory = ''.join(hashlib.sha256(p.read_bytes()).hexdigest() + '  ./' + p.relative_to(self.source).as_posix() + '\n'
                            for p in self.source.rglob('*') if p.is_file())
        (self.source / 'SHA256SUMS').write_text(inventory)
        self.download = self.root / 'download'
        self.download.mkdir()
        self.repo = self.root / 'repo'
        infrastructure = self.repo / 'infra/live-terraform'
        infrastructure.mkdir(parents=True)
        (infrastructure / 'main.tf').write_text('current infrastructure')
        (infrastructure / '.terraform.lock.hcl').write_text('locked provider')
        self.output = self.root / 'output'
        self.pack()

    def pack(self):
        archive = self.download / 'live-candidate.tar.gz'
        with tarfile.open(archive, 'w:gz') as stream:
            stream.add(self.source, arcname='.')
        (self.download / 'live-candidate.tar.gz.sha256').write_text(
            hashlib.sha256(archive.read_bytes()).hexdigest() + '  live-candidate.tar.gz\n')

    def test_reuses_bytes_and_records_distinct_source_revision(self):
        with patch.object(subprocess, 'check_output', side_effect=['', 'a' * 40 + '\n']):
            MODULE.reuse(self.download, self.output, self.repo)
        self.assertEqual((self.output / 'relay/image.oci.tar').read_bytes(),
                         (self.source / 'relay/image.oci.tar').read_bytes())
        self.assertEqual((self.output / 'infrastructure/main.tf').read_text(), 'current infrastructure')
        self.assertEqual((self.output / 'REVISION').read_text().strip(), 'a' * 40)
        self.assertIn(MODULE.SOURCE_REVISION, (self.output / 'BACKEND_SOURCE.json').read_text())
        for line in (self.output / 'SHA256SUMS').read_text().splitlines():
            digest, name = line.split('  ', 1)
            self.assertEqual(hashlib.sha256((self.output / name).read_bytes()).hexdigest(), digest)

    def test_reuses_site_without_backend_or_infrastructure_payload(self):
        for name in ('site/index.html', 'viewer/manifest.json'):
            path = self.source / name
            path.parent.mkdir(exist_ok=True)
            path.write_text('original site bytes')
        (self.source / 'tools/package-link.json').symlink_to('package.json')
        inventory = ''.join(hashlib.sha256(p.read_bytes()).hexdigest() + '  ./' + p.relative_to(self.source).as_posix() + '\n'
                            for p in self.source.rglob('*') if p.is_file() and not p.is_symlink() and p.name != 'SHA256SUMS')
        (self.source / 'SHA256SUMS').write_text(inventory)
        self.pack()
        (self.download / 'live-candidate.tar.gz').rename(self.download / 'site-candidate.tar.gz')
        checksum = (self.download / 'live-candidate.tar.gz.sha256').read_text().replace('live-candidate', 'site-candidate')
        (self.download / 'site-candidate.tar.gz.sha256').write_text(checksum)
        with patch.object(subprocess, 'check_output', side_effect=['', 'a' * 40 + '\n']):
            MODULE.reuse(self.download, self.output, self.repo, 'site')
        self.assertEqual((self.output / 'site/index.html').read_text(), 'original site bytes')
        self.assertTrue((self.output / 'SITE_SOURCE.json').exists())
        self.assertFalse((self.output / 'relay').exists())
        self.assertFalse((self.output / 'infrastructure').exists())

    def test_rejects_archive_tampering(self):
        with (self.download / 'live-candidate.tar.gz').open('ab') as stream:
            stream.write(b'tampered')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            MODULE.reuse(self.download, self.output, self.repo)

    def test_rejects_stale_product_sources(self):
        with patch.object(subprocess, 'check_output', return_value='packages/live-relay/src/main.rs\n'):
            with self.assertRaisesRegex(ValueError, 'rebuild required'):
                MODULE.reuse(self.download, self.output, self.repo)

    def test_rejects_source_inventory_tampering(self):
        (self.source / 'relay/image.oci.tar').write_text('tampered')
        self.pack()
        with patch.object(subprocess, 'check_output', return_value=''):
            with self.assertRaisesRegex(ValueError, 'inventory mismatch'):
                MODULE.reuse(self.download, self.output, self.repo)


if __name__ == '__main__':
    unittest.main()
