#!/usr/bin/env python3
"""Verify input execution on a real PTY, not shell echo, a frame, or a pane listing."""
import argparse
import fcntl
import json
import socket
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def exit_code(p, seconds):
    deadline = time.monotonic()+seconds
    while time.monotonic()<deadline:
        if p.poll() is not None:
            return p.returncode
        time.sleep(.02)
    raise RuntimeError(f'process {p.pid} did not exit')


def stop(p):
    if p is not None and p.poll() is None:
        p.terminate()
        try:
            exit_code(p, 3)
        except RuntimeError:
            p.kill()
            exit_code(p, 30)


def exercise(binary, splits, diagnostics=False):
    binary = binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix='bpt-', dir='/tmp') as name:
        root = Path(name)
        env = {'PATH': '/usr/bin:/bin', 'TERM': 'xterm-256color', 'LC_ALL': 'C',
               'BMUX_NO_BASE_CONFIG': '1', 'SHELL': '/bin/sh'}
        for var in ('HOME', 'TMPDIR', 'BMUX_CONFIG_DIR', 'BMUX_STATE_DIR', 'BMUX_DATA_DIR',
                    'BMUX_RUNTIME_DIR', 'BMUX_LOG_DIR', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME',
                    'XDG_RUNTIME_DIR', 'XDG_DATA_HOME', 'XDG_CACHE_HOME'):
            folder = root/var
            folder.mkdir(mode=0o700)
            env[var] = str(folder)
        wrapper = root/'shell'
        wrapper.write_text(f'#!/bin/sh\necho "$$" >> {root}/started\nexec /bin/sh -i\n')
        wrapper.chmod(0o700)
        env['SHELL'] = str(wrapper)
        cfg = root/'bmux.toml'
        cfg.write_text(f'[general]\ndefault_shell = "{wrapper}"\n[keybindings]\ninitial_mode = "insert"\n')
        cmd = [str(binary), '--config', str(cfg)]
        master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 120, 0, 0))
        client = server = None
        tail = b''
        logfile = root/'server.log'
        def wait(reason, condition):
            nonlocal tail
            until = time.monotonic() + 12
            while time.monotonic() < until:
                if select.select([master], [], [], .05)[0]:
                    try:
                        tail = (tail + os.read(master, 65536))[-16000:]
                    except OSError:
                        pass
                if client.poll() is not None or server.poll() is not None:
                    break
                if condition():
                    return
            raise RuntimeError(f'{reason}: client={client.poll()} server={server.poll()} PTY={tail[-1400:]!r}; server log={logfile.read_bytes()[-1400:]!r}')
        def send(data):
            os.write(master, data)
        def lines(file):
            f = root/file
            return f.read_text().splitlines() if f.exists() else []
        def marker(label):
            send(f'echo "{label}:$$" >> {root}/received\r'.encode())
            wait('shell execution '+label, lambda: any(v.startswith(label+':') for v in lines('received')))
        try:
            with logfile.open('wb') as log:
                server = subprocess.Popen(cmd+['server','start','--foreground-internal'], cwd=root, env=env,
                                          stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
            until = time.monotonic()+10
            while not list((root/'BMUX_RUNTIME_DIR').rglob('*.sock')):
                if server.poll() is not None or time.monotonic()>until:
                    raise RuntimeError('server startup: '+repr(logfile.read_bytes()[-1400:]))
                time.sleep(.05)
            if diagnostics:
                settings = subprocess.check_output(cmd+['perf','status','--json'], cwd=root, env=env, timeout=15)
                if 'recording_level' not in json.loads(settings):
                    raise RuntimeError('diagnostic service did not execute')
            if not splits:
                # No network service and no real credentials. Bind without listening
                # so the loopback port is reserved but the selected target refuses.
                with socket.socket() as reserved:
                    reserved.bind(('127.0.0.1', 0))
                    remote_cfg = root/'remote.toml'
                    remote_cfg.write_text(cfg.read_text()+f"[connections]\ndefault_target = 'tls://127.0.0.1:{reserved.getsockname()[1]}'\n")
                    denied = subprocess.run(cmd[:-1]+[str(remote_cfg)], cwd=root, env=env,
                                            stdin=subprocess.DEVNULL, capture_output=True, timeout=15)
                    if (b'TLS target' not in denied.stderr or
                            f'127.0.0.1:{reserved.getsockname()[1]}'.encode() not in denied.stderr):
                        raise RuntimeError(f'unexpected target failure: {denied.stderr[-600:]!r}')
                    if denied.returncode == 0 or lines('started'):
                        raise RuntimeError('remote selection fell back to local execution')
            # Keep the controlling terminal alive until stty samples restored modes.
            # Darwin revokes the slave when its session leader exits.
            supervisor = 'stty -g > before; "$@" <&0 & pid=$!; echo $pid > attacher; wait "$pid"; rc=$?; stty -g > after; echo "$rc" > status; exit "$rc"'
            def finish_attach(code):
                nonlocal master, slave
                deadline = time.monotonic()+15
                while not lines('status') and time.monotonic()<deadline:
                    time.sleep(.02)
                if lines('status') != [str(code)] or not lines('after') or lines('after') != lines('before'):
                    raise RuntimeError(f'client exit/restoration mismatch: {lines("status")}, expected {code}')
                # Darwin can block session-leader tty teardown while the harness
                # holds the PTY open; close it after sampling the child status.
                os.close(slave); os.close(master); slave = master = None
                if exit_code(client, 15) != code:
                    raise RuntimeError('wrapper exit mismatch')
                (root/'status').unlink()
            def attach():
                return subprocess.Popen(cmd if splits else ["/bin/sh","-c",supervisor,"sh",*cmd], cwd=root, env=env, stdin=slave, stdout=slave, stderr=slave,
                                        preexec_fn=controlling_terminal)
            def raw():
                return not termios.tcgetattr(slave)[3] & termios.ICANON
            client = attach()
            wait('initial shell and attached input loop', lambda: lines('started') and raw() and (not splits or b'\x1b[?2026l' in tail))
            marker('first')
            first = lines('received')[0].split(':')[1]
            if first not in lines('started'):
                raise RuntimeError('input did not execute in the configured shell')
            if splits:
                send(b'\x1bn')  # Alt+n: split the focused pane
                wait('second shell spawn', lambda: len(set(lines('started'))) == 2)
                marker('second')
                second = lines('received')[-1].split(':')[1]
                if second == first or second not in lines('started'):
                    raise RuntimeError(f'split input did not reach the new shell: first={first}, second={second}')
                send(b'\x1bt')  # Alt+t: focus the next pane (back to the first)
                marker('back')
                expected = [f'first:{first}', f'second:{second}', f'back:{first}']
                if lines('received') != expected:
                    raise RuntimeError(f'input routing: expected {expected}, got {lines("received")}')
            else:
                for trigger,label,code in [('detach','reattach',0), ('terminate','reconnect',143)]:
                    if trigger == 'detach':
                        send(b'\x1d')
                    else:
                        os.kill(int(lines('attacher')[0]), signal.SIGTERM)
                    finish_attach(code)
                    master, slave = os.openpty()
                    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 120, 0, 0))
                    client = attach()
                    wait('reattached raw mode', raw)
                    marker(label)
                    if lines('received')[-1] != label+':'+first or lines('started') != [first]:
                        raise RuntimeError('reattachment replaced the execution')
                send(b'exit 7\r')
                finish_attach(7)
            received = lines('received')
        finally:
            try:
                stop(client)
            finally:
                try:
                    stop(server)
                finally:
                    if slave is not None: os.close(slave)
                    if master is not None: os.close(master)
        print(f'PASS {binary}: {received}', flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('bare', type=Path)
    parser.add_argument('local_splits', type=Path)
    parser.add_argument('--diagnostics', type=Path)
    args = parser.parse_args()
    failed = False
    for binary, splits in [(args.bare, False), (args.local_splits, True), (args.diagnostics, False)]:
        if binary is None: continue
        try:
            exercise(binary, splits, binary == args.diagnostics)
        except (OSError, RuntimeError) as err:
            print(f'FAIL {binary}: {err}', flush=True)
            failed = True
    raise SystemExit(int(failed))
