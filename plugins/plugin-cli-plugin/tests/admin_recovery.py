#!/usr/bin/env python3
"""Exercise the real bundled admin commands, without executing the fixture plugin."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def exercise(binary, full):
    with tempfile.TemporaryDirectory(prefix='bmux-plugin-admin-', dir='/tmp') as directory:
        root = Path(directory).resolve()
        config = root / 'config/bmux.toml'
        config.parent.mkdir()
        plugin = root / 'plugins/fixture'
        plugin.mkdir(parents=True)
        entry = plugin / 'worker'
        entry.write_text('#!/bin/sh\nexit 97\n')
        entry.chmod(0o600)
        (plugin / 'plugin.toml').write_text('id="fixture.admin"\nname="Admin fixture"\n'
                                             'version="0.1.0"\nruntime="process"\nentry="./worker"\n')
        config.write_text('[plugins]\nenabled=["fixture.admin"]\nsearch_paths=[' +
                          json.dumps(str(plugin.parent)) + ']\n')
        env = {'HOME': directory, 'PATH': str(root / 'empty'), 'TERM': 'dumb', 'TMPDIR': directory,
               'BMUX_LOG_LEVEL': 'error', 'BMUX_CONFIG': str(config), 'NO_COLOR': '1'}
        for kind in ('config', 'data', 'state', 'runtime', 'log'):
            env['BMUX_' + kind.upper() + '_DIR'] = str(root / kind)
        for kind in ('data', 'state', 'config', 'cache'):
            env['XDG_' + kind.upper() + '_HOME'] = str(root / kind)

        def run(*args, status=0):
            p = subprocess.run([binary, *args], env=env, cwd=root, stdin=subprocess.DEVNULL,
                               capture_output=True, timeout=20, text=True, check=False)
            assert p.returncode == status, (args, p.returncode, p.stderr, p.stdout[:300])
            return p.stdout

        def findings(*args, status):
            report = json.loads(run('plugin', 'doctor', '--json', *args, status=status))
            return {f['code'] for f in report['findings'] if 'fixture.admin' in f['message']}

        assert 'process_entry_not_executable' in findings(status=1)
        entry.chmod(0o700)
        assert findings(status=0) == {'process_runtime_detected', 'process_stdout_reserved'}
        if full:
            entries = json.loads(run('plugin', 'list', '--json', '--enabled-only'))
            assert all(e['enabled'] for e in entries) and any(e['id'] == 'fixture.admin' for e in entries)
            assert not findings('--severity', 'error', '--strict', status=0)
            config.write_text('[plugins]\nenabled=["fixture.missing"]\n')
            report = json.loads(run('--core-builtins-only', 'doctor', '--json', status=1))
            assert 'fixture.missing' in report['plugins']['scan_error']
            # Generic `config set` accepts scalar values, not TOML arrays. Edit
            # the file named by config path; never replace an unknown user file.
            selected = json.loads(run('--core-builtins-only', 'config', 'path', '--json'))
            assert selected['path'] == str(config), selected
            config.write_text('[plugins]\nenabled=[]\n')
            assert not findings('--strict', status=0)
            assert run('plugin', 'run', 'bmux.plugin_cli', 'list', status=1) == ''
            config.write_text('[plugins]\ndisabled=["bmux.plugin_cli"]\n')
            run('plugin', 'list', '--json', status=1)
            assert json.loads(run('--core-builtins-only', 'config', 'path', '--json'))['path'] == str(config)
        print('PASS: non-executable process diagnosed; executable-bit repair verified' +
              ('; registry recovery and list/doctor filters verified' if full else ''))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--baseline', action='store_true', help='only exercise baseline admin commands')
    args = parser.parse_args()
    exercise(args.binary.resolve(strict=True), not args.baseline)
