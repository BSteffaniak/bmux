#!/usr/bin/env python3
"""Temporary migration-only backend reuse from one pinned CI candidate."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

SOURCE_REVISION = 'e84e1b69a4f59e8174e5a4ac8aa7af18f207d480'
SOURCE_RUN = 39048863096


def reuse(download, output, root, kind='backend'):
    if kind not in ('backend', 'site'):
        raise ValueError('Invalid reuse kind')
    archive_name = 'site-candidate' if kind == 'site' else 'live-candidate'
    if output.exists():
        raise ValueError('Output must not exist')
    archive = download / (archive_name + '.tar.gz')
    checksum = (download / (archive_name + '.tar.gz.sha256')).read_text().split()
    with archive.open('rb') as stream:
        actual = hashlib.file_digest(stream, 'sha256').hexdigest()
    if len(checksum) != 2 or checksum[1] != archive_name + '.tar.gz' or checksum[0] != actual:
        raise ValueError('Source archive checksum mismatch')
    # Do not silently reuse stale binaries after product or dependency changes.
    changed = subprocess.check_output(['git', '-C', str(root), 'diff', '--name-only',
        SOURCE_REVISION, 'HEAD', '--', 'packages', 'plugins', 'Cargo.toml', 'Cargo.lock',
        'flake.nix', 'flake.lock', *(['scripts/build-site-artifact.sh',
            'scripts/build-web-renderer.sh', 'scripts/presenter-adapter.js'] if kind == 'site' else [])], text=True)
    if changed.strip():
        raise ValueError('Backend sources changed since pinned build; rebuild required')
    with tempfile.TemporaryDirectory() as temporary:
        source = Path(temporary)
        with tarfile.open(archive) as bundle:
            members = bundle.getmembers()
            if any(not (m.isfile() or m.isdir() or (kind == 'site' and (m.issym() or m.islnk()))) or Path(m.name).is_absolute()
                   or '..' in Path(m.name).parts for m in members):
                raise ValueError('Unsafe candidate archive')
            bundle.extractall(source, members=members, filter='data')
        for path in source.rglob('*'):
            if path.is_symlink() and (not path.resolve().is_relative_to(source) or not path.resolve().exists()):
                raise ValueError('Unsafe candidate link')
        if (source / 'REVISION').read_text().strip() != SOURCE_REVISION:
            raise ValueError('Unexpected source revision')
        verified = set()
        for line in (source / 'SHA256SUMS').read_text().splitlines():
            digest, name = line.split('  ', 1)
            path = source / name
            if not path.resolve().is_relative_to(source) or not re.fullmatch('[a-f0-9]{64}', digest):
                raise ValueError('Invalid source inventory')
            with path.open('rb') as stream:
                if hashlib.file_digest(stream, 'sha256').hexdigest() != digest:
                    raise ValueError('Source inventory mismatch')
            verified.add(path.resolve())
        actual_files = {p.resolve() for p in source.rglob('*') if p.is_file() and p.name != 'SHA256SUMS'}
        if verified != actual_files:
            raise ValueError('Source inventory incomplete')
        output.mkdir(parents=True)
        for name in (('site', 'viewer', 'tools') if kind == 'site' else ('native', 'relay', 'tools')):
            shutil.copytree(source / name, output / name)
        if kind == 'backend':
            infrastructure = output / 'infrastructure'
            infrastructure.mkdir()
            for path in (root / 'infra/live-terraform').glob('*.tf'):
                shutil.copyfile(path, infrastructure / path.name)
            shutil.copyfile(root / 'infra/live-terraform/.terraform.lock.hcl', infrastructure / '.terraform.lock.hcl')
        revision = subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True)
        (output / 'REVISION').write_text(revision)
        provenance = {'run_id': SOURCE_RUN, 'revision': SOURCE_REVISION, 'archive_sha256': actual}
        if kind == 'backend':
            provenance['image'] = (output / 'relay/IMAGE').read_text().strip()
        (output / (kind.upper() + '_SOURCE.json')).write_text(json.dumps(provenance, sort_keys=True) + '\n')
        lines = []
        for path in sorted(output.rglob('*')):
            if path.is_file():
                with path.open('rb') as stream:
                    digest = hashlib.file_digest(stream, 'sha256').hexdigest()
                lines.append(digest + '  ./' + path.relative_to(output).as_posix() + '\n')
        (output / 'SHA256SUMS').write_text(''.join(lines))


if __name__ == '__main__':
    reuse(Path(sys.argv[1]), Path(sys.argv[2]), Path(__file__).resolve().parents[1],
          sys.argv[3] if len(sys.argv) > 3 else 'backend')
