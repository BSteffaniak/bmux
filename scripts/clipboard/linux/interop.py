#!/usr/bin/env python3
"""Run against a lane_probe binary, never against an inherited display."""
import argparse
import contextlib
import hashlib
import os
from pathlib import Path
import select
import signal
import subprocess as sp
import sys
import tempfile
import time

from PIL import Image


def digest(data):
    return hashlib.sha256(data).hexdigest()


@contextlib.contextmanager
def process(args, **kw):
    child = sp.Popen(args, start_new_session=True, **kw)
    try:
        yield child
    finally:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=5)


def line(pipe):
    if not select.select([pipe], [], [], 10)[0]:
        raise TimeoutError('readiness timeout')
    return pipe.readline().strip()


@contextlib.contextmanager
def display(protocol, root):
    runtime = root / 'runtime'
    runtime.mkdir(mode=0o700)
    env = dict(HOME=str(root), XDG_RUNTIME_DIR=str(runtime), PATH=os.environ['PATH'],
               LANG='C.UTF-8', BMUX_CLIPBOARD_ISOLATED_TEST='1')
    if protocol == 'x11':
        auth = root / 'Xauthority'
        auth.touch(mode=0o600)
        env['XAUTHORITY'] = str(auth)
        cookie = os.urandom(16).hex()
        def authorize(name):
            sp.run(['xauth', 'add', name, '.', cookie], env=env, check=True, timeout=5)
        authorize(':65535')  # server loads the cookie, not a predetermined display
        with process(['Xvfb', '-auth', str(auth), '-nolisten', 'tcp', '-displayfd', '1'],
                     env=env, stdout=sp.PIPE) as server:
            env['DISPLAY'] = ':' + str(int(line(server.stdout)))
            authorize(env['DISPLAY'])
            yield env
    else:
        config = root / 'sway.conf'
        config.write_text('xwayland disable\nseat seat0 fallback true\n')
        env.update(WLR_BACKENDS='headless', WLR_RENDERER='pixman', WLR_LIBINPUT_NO_DEVICES='1')
        with process(['sway', '-c', str(config)], env=env) as server:
            for _ in range(200):
                sockets = [s for s in runtime.glob('wayland-*') if s.is_socket()]
                if sockets:
                    env['WAYLAND_DISPLAY'] = sockets[0].name
                    break
                if server.poll() is not None:
                    raise RuntimeError('headless sway exited')
                time.sleep(.05)
            else:
                raise TimeoutError('no private compositor socket')
            yield env


def case(probe, protocol, kind, env, root):
    source, replacement, result = (root / f for f in ('source', 'replacement', 'result'))
    mime = 'image/png' if kind == 'image' else (
        'UTF8_STRING' if protocol == 'x11' else 'text/plain;charset=utf-8')
    if kind == 'image':
        for file, color in [(source, (2, 3, 4, 128)), (replacement, (9, 7, 6, 200))]:
            Image.new('RGBA', (3, 2), color).save(file, 'PNG')
        def normalize(file):
            with Image.open(file) as img:
                assert img.size == (3, 2)
                return img.convert('RGBA').tobytes()
    else:
        for file in (source, replacement):
            file.write_bytes((file.name + ' 界\n').encode() * 60000)
        normalize = Path.read_bytes
    if protocol == 'x11':
        base = ['xclip', '-selection', 'clipboard', '-target', mime]
        read, write = base + ['-out'], base + ['-in', '-quiet']
    else:
        read = ['wl-paste', '--no-newline', '--type', mime]
        write = ['wl-copy', '--foreground', '--type', mime]
    # Independent commands retain their own PATH; product cannot find helpers.
    native = dict(env, PATH=str(root / 'no-helpers'))
    with process([probe, 'write-' + kind, source], env=native, stdin=sp.PIPE, stdout=sp.PIPE) as owner:
        if line(owner.stdout) != b'ready':
            raise RuntimeError('writer did not acquire selection')
        result.write_bytes(sp.check_output(read, env=env, timeout=12))
        if normalize(result) != normalize(source):
            raise ValueError('outbound payload mismatch')
        print(protocol, kind, 'outbound', digest(normalize(result)), flush=True)
        with replacement.open('rb') as stream, process(write, env=env, stdin=stream) as peer:
            # Wait for external ownership, then exit the superseded writer. It must
            # not reclaim or clear the new owner's selection during destruction.
            until = time.monotonic() + 12
            while time.monotonic() < until and peer.poll() is None:
                capture = sp.run(read, env=env, stdout=sp.PIPE, timeout=12)
                if capture.returncode == 0:
                    result.write_bytes(capture.stdout)
                    if normalize(result) == normalize(replacement):
                        break
                time.sleep(.05)
            else:
                raise RuntimeError('replacement did not acquire selection')
            owner.stdin.close()
            if owner.wait(timeout=10) != 0:
                raise RuntimeError('previous writer exit failed')
            result.unlink()
            sp.run([probe, 'read-' + kind, result], env=native, check=True, timeout=12)
            if normalize(result) != normalize(replacement):
                raise ValueError('inbound payload mismatch')
            print(protocol, kind, 'inbound-after-replacement', digest(normalize(result)), flush=True)
            if kind == 'text':
                os.kill(peer.pid, signal.SIGSTOP)
                try:
                    stalled = sp.run([probe, 'read-text', result], env=native, timeout=18)
                    if stalled.returncode == 0:
                        raise ValueError('stalled peer reported successful read')
                    print(protocol, 'stalled-peer bounded error', flush=True)
                finally:
                    os.kill(peer.pid, signal.SIGCONT)
    # No clipboard manager: measure whether an independent reader can paste the
    # writer's value after normal exit; don't assume reference persistence.
    with process([probe, 'write-' + kind, source], env=native, stdin=sp.PIPE, stdout=sp.PIPE) as owner:
        if line(owner.stdout) != b'ready':
            raise RuntimeError('second writer did not acquire selection')
        owner.stdin.close()
        if owner.wait(timeout=10) != 0:
            raise RuntimeError('owner exit failed')
        capture = sp.run(read, env=env, stdout=sp.PIPE, timeout=12)
        if capture.returncode == 0:
            result.write_bytes(capture.stdout)
            if normalize(result) != normalize(source):
                raise ValueError('post-exit paste returned different data')
        print(protocol, kind, 'post-exit-peer-status', capture.returncode, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('probe', type=Path)
    args = parser.parse_args()
    probe = args.probe.resolve(strict=True)
    print('probe-sha256', digest(probe.read_bytes()), flush=True)
    failures = 0
    for protocol in ('x11', 'wayland'):
        for kind in ('text', 'image'):
            try:
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    with display(protocol, root) as env:
                        case(probe, protocol, kind, env, root)
            except Exception as error:
                failures += 1
                print(protocol, kind, 'FAIL', error, file=sys.stderr, flush=True)
    return bool(failures)


if __name__ == '__main__':
    sys.exit(main())
