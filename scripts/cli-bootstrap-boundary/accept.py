#!/usr/bin/env python3
"""Local acceptance: pass one or more explicitly built bmux images (including minimal)."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def exercise(image, disabled):
    with tempfile.TemporaryDirectory(dir='/tmp', prefix='bboot-') as path:
        root = Path(path)
        env = {'PATH': '/usr/bin:/bin', 'TERM': 'dumb', 'LC_ALL': 'C', 'HOME': path}
        for key in ('BMUX_CONFIG_DIR', 'BMUX_STATE_DIR', 'BMUX_RUNTIME_DIR', 'BMUX_DATA_DIR',
                    'BMUX_LOG_DIR', 'BMUX_SLOTS_BIN_DIR', 'TMPDIR', 'XDG_CONFIG_HOME',
                    'XDG_STATE_HOME', 'XDG_DATA_HOME', 'XDG_RUNTIME_DIR', 'XDG_CACHE_HOME'):
            folder = root/key.lower()
            folder.mkdir(mode=0o700)
            env[key] = str(folder)
        config = Path(env['BMUX_CONFIG_DIR'])/'bmux.toml'
        config.write_text('[plugins]\ndisabled = '+json.dumps(disabled)+'\n')
        def run(*args, status=0):
            p = subprocess.run([image, *args], cwd=root, env=env, capture_output=True,
                               stdin=subprocess.DEVNULL, timeout=30)
            if p.returncode != status:
                raise AssertionError((args, p.returncode, p.stderr))
            return p.stdout
        location = json.loads(run('config','path','--json'))
        assert Path(location['path']).resolve() == config.resolve() and location['exists']
        assert run('config','set','recording.enabled','false') == b'recording.enabled = false\n'
        assert json.loads(run('config','get','recording.enabled','--json')) is False
        assert run('slot','shell','--shell','posix') == run('env','shell','--shell','posix')
        assert b'BMUX_SLOTS_BIN_DIR' in run('slot','shell','--shell','posix')
        listing = json.loads(run('sandbox','list','--json'))
        assert listing['schema_version'] == 1 and listing['sandboxes'] == []
        status = json.loads(run('server','status','--json', status=1))
        assert status['running'] is False and status['principal_id'] is None
        assert b'--foreground-internal' not in run('server','start','--help')
        assert run('server','bridge','--stdio','--preflight') == b'BMUX_BRIDGE_READY\n'
        assert run('server','start','--rolling-recording','--no-rolling-recording',status=2) == b''
        assert run('sandbox','inspect','--latest','--latest-failed',status=2) == b''
        print(image, 'explicitly disabled' if disabled else 'defaults', 'PASS')

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('images', nargs='+', type=Path)
    for image in parser.parse_args().images:
        for disabled in ([], ['bmux.sessions','bmux.contexts','bmux.clients','bmux.tabs',
                              'bmux.recording','bmux.connections','bmux.snapshot']):
            exercise(image.resolve(strict=True), disabled)
