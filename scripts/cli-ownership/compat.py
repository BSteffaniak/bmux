"""Capture/compare byte-exact CLI transcripts using already-built CLI binaries."""
import argparse
import base64
import difflib
import gzip
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from dispositions import AUDIT, MIGRATED, disposition
from ownership import FIXTURE, HERE, REV, ROOT, inventory
import tomllib

MODES = ('bare', 'local-splits', 'default', 'disabled')
MANIFEST = json.loads(FIXTURE.read_text())


def check_catalog():
    current = inventory()['paths']
    migrated = {p: owner for owner, paths in MIGRATED.items() for p in paths}
    paths = set(current) | set(MANIFEST['paths'])
    paths |= {' '.join(p.split()[:i]) for p in paths for i in range(1, len(p.split()))}
    if paths != AUDIT.keys():
        raise ValueError(f'unclassified/stale paths: {sorted(paths ^ AUDIT.keys())}')
    declared = {}
    for file in sorted((ROOT / 'plugins').glob('*/plugin.toml')):
        manifest = tomllib.loads(file.read_text())
        for cmd in manifest.get('commands', []):
            canonical = cmd.get('path') or [cmd['name']]
            for words in [canonical, *cmd.get('aliases', [])]:
                declared.setdefault(' '.join(words), []).append(dict(
                    owner=manifest['id'], command=cmd['name'], manifest=str(file.relative_to(ROOT)),
                    canonical=' '.join(canonical), exposed=cmd.get('expose_in_cli', False)))
    for path, row in AUDIT.items():
        entry, ds = current.get(path, {}), declared.get(path, [])
        if (row.get('declarations', []) != ds or row.get('schema') != entry.get('schema')
                or row.get('core') != entry.get('core')
                or row.get('core_callers', []) != entry.get('called_by_proxy', [])):
            raise ValueError(f'{path}: command disposition no longer matches source')
        if entry.get('schema') or entry.get('core'):
            kind, owner = 'retain-core', 'core'
        elif path in migrated:
            kind, owner = 'migrated', migrated[path]
        elif ds:
            kind = 'retain-plugin' if any(d['exposed'] for d in ds) else 'non-cli'
            owner = ds[0]['owner']
        else:
            kind, owner = 'composed-namespace', None
            owners = sorted({d['owner'] for p, items in declared.items() if p.startswith(path + ' ')
                             for d in items if d['exposed']})
            if not owners or owners != row.get('owners'):
                raise ValueError(f'{path}: namespace owner set changed')
        if (kind, owner) != (row['disposition'], row.get('owner')):
            raise ValueError(f'{path}: expected {kind} / {owner}')


def cases():
    yield ('--help',)
    # `help PATH` probes availability without invoking an external command,
    # including when its plugin is absent. Keep absent-path errors as evidence.
    for path in MANIFEST['paths']:
        yield ('help', *path.split())
    # Direct parser vs composed help; namespace siblings, hidden entries, aliases,
    # default values, repeated options, conflict/arity errors, safe local effects.
    for argv in [
        'remote list --help', 'remote trust list --help', 'server status --help',
        'server recording --help', 'playbook validate --help', 'playbook from-recording --help',
        'session clients --help', 'env print --help', 'access init --key-file a --key-file b --help',
        'share --help', 'share revoke', 'share --role', 'unshare', 'host --daemon --stop',
        'remote trust remove', 'playbook validate missing-fixture --json',
        'config path', 'remote complete targets', 'logs profiles list --json',
        '--runtime invalid/name --help', 'share revoke name extra',
    ]:
        yield tuple(argv.split())


def transcript(binary, mode, probes=None):
    result = {}
    for argv in cases() if probes is None else probes:
        with tempfile.TemporaryDirectory(prefix='bmux-ref-', dir='/tmp') as name:
            root = Path(name).resolve()
            config = root / 'config.toml'
            disabled = sorted(MANIFEST['claims']) if mode == 'disabled' else []
            config.write_text('[plugins]\ndisabled = ' + json.dumps(disabled) + '\n')
            env = {'HOME': str(root), 'PATH': str(root / 'empty-bin'), 'LC_ALL': 'C',
                   'BMUX_LOG_LEVEL': 'error', 'NO_COLOR': '1', 'TERM': 'dumb', 'BMUX_CONFIG': str(config),
                   'TMPDIR': str(root), 'TOKIO_WORKER_THREADS': '1'}
            for kind in ('config', 'runtime', 'data', 'state', 'log'):
                env['BMUX_' + kind.upper() + '_DIR'] = str(root / kind)
            for kind in ('config', 'data', 'state', 'cache'):
                env['XDG_' + kind.upper() + '_HOME'] = str(root / kind)
            # No inherited live config, agent, proxy, credentials, PATH helpers or tty.
            proc = subprocess.run([binary, *argv], env=env, cwd=root, input=b'',
                                  capture_output=True, timeout=15)
            def stream(data):
                # Normalize only the per-case private directory, preserving all other bytes.
                for spelling in (str(root), name):
                    data = data.replace(spelling.encode(), b'<fixture>')
                # Debug Clap may panic on baseline descriptor collisions. Preserve
                # exit 101 and the panic; thread IDs and registry roots are machine-local.
                if proc.returncode == 101:
                    data = re.sub(rb"(thread 'main') \(\d+\)( panicked at )[^\n]+?/clap_builder-", rb'\1 (<tid>)\2<registry>/clap_builder-', data)
                return base64.b64encode(data).decode('ascii')
            result[' '.join(argv)] = [proc.returncode, stream(proc.stdout), stream(proc.stderr)]
    return result


def compare(expected, actual, mode):
    issues = 0
    for case in sorted(expected.keys() | actual.keys()):
        a, b = expected.get(case), actual.get(case)
        policy = disposition(case, mode, a, b)
        if policy not in (None, 'violation'):
            print(f'{case}: policy: {policy}')
            continue
        if a == b and policy is None:
            continue
        issues += 1
        print(f'{case}: expected/actual exit: {a and a[0]}/{b and b[0]}')
        if a and b:
            for i, channel in [(1, 'stdout'), (2, 'stderr')]:
                aa, bb = (base64.b64decode(val[i]).decode('utf-8', 'backslashreplace').splitlines(True) for val in (a, b))
                sys.stdout.writelines(difflib.unified_diff(aa, bb, 'old '+channel, 'new '+channel))
    return issues


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['capture', 'check'])
    parser.add_argument('mode', choices=MODES)
    parser.add_argument('binary', type=Path, help='explicit bmux binary for this profile')
    parser.add_argument('--fixtures', type=Path, default=HERE / 'transcripts')
    args = parser.parse_args()
    snapshot = args.fixtures / (args.mode + '.json.gz')
    if args.action == 'capture' and snapshot.exists():
        parser.error('refusing to overwrite a reference transcript')
    # A structural classification is not a runtime waiver. Reject a stale
    # catalog before comparing binaries; the owner map includes aliases and
    # implicit parent paths even where no historical transcript exists.
    if args.action == 'check':
        check_catalog()
    actual = transcript(args.binary.resolve(strict=True), args.mode)
    if args.action == 'check':
        for case, (status, out, _) in actual.items():
            if status or not (case == '--help' or case.startswith('help ')):
                continue
            parent = '' if case == '--help' else case[5:] + ' '
            block = base64.b64decode(out).partition(b'\nCommands:\n')[2].partition(b'\n\n')[0]
            for name in re.findall(rb'^  ([\w-]+)(?:  |$)', block, re.M):
                if name != b'help' and parent + name.decode() not in AUDIT:
                    raise SystemExit(f'{case}: unclassified advertised child {name.decode()}')
    if args.action == 'capture':
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        with snapshot.open('xb') as f:
            f.write(gzip.compress(json.dumps({'revision': REV, 'mode': args.mode, 'cases': actual}, sort_keys=True).encode(), mtime=0))
    else:
        doc = json.loads(gzip.decompress(snapshot.read_bytes()))
        if (doc['revision'], doc['mode']) != (REV, args.mode):
            raise SystemExit('wrong reference fixture')
        if count := compare(doc['cases'], actual, args.mode):
            raise SystemExit(f'{count} transcript mismatch(es)')
    print(f'{args.mode}: {len(actual)} transcripts {args.action} OK')
