#!/usr/bin/env python3
"""Local Unix acceptance test. Pass the absolute path to the Nix-built bmux."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import time


def assert_attached(binary, env, root, session, session_id, run, capture=False):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))
    # Give the client a real controlling terminal, not just three tty fds.
    def terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    try:
        proc = subprocess.Popen([binary, 'session', 'attach', session], cwd=root, env=env,
                                stdin=slave, stdout=slave, stderr=slave, preexec_fn=terminal)
    finally:
        os.close(slave)
    try:
        deadline = time.monotonic() + 55
        captured = bytearray()
        next_poll = 0
        while time.monotonic() < deadline:
            if select.select([master], [], [], .25)[0]:
                chunk = os.read(master, 16384)
                if not chunk:
                    break
                captured.extend(chunk)
                del captured[:-131072]
            if proc.poll() is not None:
                break
            if time.monotonic() >= next_poll:
                # The server, not the presence of an ANSI escape sequence, is
                # authoritative about which session received this attachment.
                clients = json.loads(run('list-clients', '--json'))
                if any(c.get('selected_session_id') == session_id for c in clients):
                    if capture:
                        # Capture rendered output, not only the attach handshake.
                        # Stop while attached: the capture writer can drain
                        # before the PTY is closed and its client goes away.
                        os.write(master, b"printf 'source-%s\\n' sentinel\r")
                        time.sleep(1)
                        stopped = run('recording', 'stop')
                        end = time.monotonic() + 2
                        while time.monotonic() < end:
                            if select.select([master], [], [], .2)[0]:
                                os.read(master, 16384)
                        return stopped
                    return
                next_poll = time.monotonic() + 1
        raise AssertionError(f'no live attachment to {session_id}: {bytes(captured[-500:])!r}')
    finally:
        os.close(master)
        if proc.poll() is None:
            proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)


def verify(binary, root, workflow):
    env = {k: os.environ[k] for k in ('PATH', 'LANG', 'LC_ALL') if k in os.environ}
    env.update(HOME=str(root), TMPDIR=str(root), SHELL='/bin/sh', TERM='xterm-256color',
               BMUX_PLAYBOOK_ENV_MODE='inherit')
    for key in ('CONFIG', 'DATA', 'STATE', 'LOG', 'RUNTIME'):
        directory = root / key.lower()
        directory.mkdir(mode=0o700)
        env[f'BMUX_{key}_DIR'] = str(directory)
    def run(*args):
        r = subprocess.run([binary, *args], cwd=root, env=env, capture_output=True,
                           text=True, timeout=90, stdin=subprocess.DEVNULL)
        if r.returncode:
            raise AssertionError(f'{args}: exit {r.returncode}\n{r.stderr[-1800:]}\n{r.stdout[-500:]}')
        return r.stdout
    session = 'roundtrip'
    try:
        run('server', 'start', '--daemon')
        run('new-session', session)
        if workflow == 'recording':
            run('recording', 'start', '--name', 'source', '--profile', 'full')
            session_id = next(s['id'] for s in json.loads(run('list-sessions', '--json')) if s['name'] == session)
            stopped = assert_attached(binary, env, root, session, session_id, run, capture=True)
            recording = re.search(r'recording stopped: ([0-9a-f-]{36})', stopped)[1]
            run('playbook', 'from-recording', recording, '--output', 'generated.dsl')
            dsl = (root / 'generated.dsl').read_text()
            assert 'wait-for pattern=' in dsl, dsl
            # Capture began in an existing shell. Supply the same precondition
            # explicitly; conversion must not invent a session creation event.
            (root / 'replay.dsl').write_text('new-session\n' + dsl)
            replay = json.loads(run('playbook', 'run', '--json', 'replay.dsl'))
            assert replay['pass'] and any(step['action'] == 'wait-for' and step['status'] == 'pass'
                                          for step in replay['steps']), replay
            run('recording', 'export', recording, '--output', str(root / 'export.gif'))
            gif = (root / 'export.gif').read_bytes()
            assert gif[:6] in (b'GIF87a', b'GIF89a') and gif[-1:] == b';'
            assert all(struct.unpack('<HH', gif[6:10]))
            print('record -> from-recording -> replay recorded wait-for; recording export -> GIF: PASS', flush=True)
            return
        session_id = next(s['id'] for s in json.loads(run('list-sessions', '--json')) if s['name'] == session)
        path = Path(run('server', 'save').strip().removeprefix('snapshot saved: '))
        assert path.resolve().is_relative_to(root) and path.is_file()
        saved = path.read_bytes()
        run('server', 'stop')
        # Prove restore is needed: start without the snapshot, then put the exact
        # saved bytes back only after verifying the new server has no such session.
        path.unlink(missing_ok=True)
        run('server', 'start', '--daemon')
        assert session not in run('list-sessions'), 'session survived without snapshot'
        # Let the empty server finish its startup checkpoint before installing
        # the backup. Apply once: startup must not already have restored it.
        time.sleep(2)
        restore_file = path.with_suffix('.restore')
        restore_file.write_bytes(saved)
        restore_file.replace(path)
        assert 'failed_plugins=0' in run('server', 'restore', '--yes')
        assert session in run('list-sessions')
        assert not any(c.get('selected_session_id') == session_id for c in json.loads(run('list-clients', '--json')))
        assert_attached(binary, env, root, session, session_id, run)
        print('save -> empty server -> restore -> server-confirmed PTY attach to original session UUID: PASS', flush=True)
    finally:
        run('server', 'stop')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--workflow', choices=('recording', 'snapshot', 'both'), default='both')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    for workflow in (('recording', 'snapshot') if args.workflow == 'both' else (args.workflow,)):
        with tempfile.TemporaryDirectory(prefix='brt-', dir='/tmp') as tmp:
            verify(binary, Path(tmp).resolve(), workflow)
