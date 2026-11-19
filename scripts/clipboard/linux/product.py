#!/usr/bin/env python3
"""Drive the actual Bterm window. No clipboard calls occur outside our private X server."""
import contextlib
import hashlib
import os
from pathlib import Path
import secrets
import select
import shutil
import signal
import subprocess as sp
import sys
import tempfile
import time


def wait_for(label, check, seconds=25):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = check()
        if value:
            return value
        time.sleep(.1)
    raise TimeoutError(label)


@contextlib.contextmanager
def process(args, env, root, log, **kwargs):
    with (root / log).open('ab') as output:
        child = sp.Popen(args, env=env, cwd=root, start_new_session=True,
                         stderr=output, stdout=kwargs.pop('stdout', output), **kwargs)
        try:
            yield child
        finally:
            try:
                os.killpg(child.pid, signal.SIGTERM)
                child.wait(timeout=5)
            except sp.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)
            except ProcessLookupError:
                child.wait(timeout=5)


def main():
    binaries = Path(sys.argv[1]).resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix='bt-clip-', dir='/tmp'))
    print('private logs and evidence:', root, flush=True)
    for folder in ['home', 'config', 'data', 'state', 'runtime', 'logs', 'bin', 'tmp']:
        (root / folder).mkdir(mode=0o700)
    # Product PATH has no clipboard helpers. Only the test driver can launch xclip.
    bash = shutil.which('bash')
    for name, path in [('bash', bash), ('sh', bash), ('bmux', binaries / 'bmux')]:
        (root / 'bin' / name).symlink_to(path)
    env = {k: os.environ[k] for k in ('LD_LIBRARY_PATH', 'VK_DRIVER_FILES', 'FONTCONFIG_FILE',
                                      'FONTCONFIG_PATH', 'XDG_DATA_DIRS') if k in os.environ}
    env.update(HOME=str(root/'home'), PATH=os.environ['PATH'], LANG='C.UTF-8',
               XDG_CONFIG_HOME=str(root/'config'), XDG_DATA_HOME=str(root/'data'),
               XDG_STATE_HOME=str(root/'state'), XDG_RUNTIME_DIR=str(root/'runtime'),
               BMUX_CONFIG_DIR=str(root/'config'), BMUX_DATA_DIR=str(root/'data'),
               BMUX_RUNTIME_DIR=str(root/'runtime'), BMUX_STATE_DIR=str(root/'state'),
               BMUX_LOG_DIR=str(root/'logs'), TMPDIR=str(root/'tmp'),
               XAUTHORITY=str(root/'auth'), WGPU_BACKEND='vulkan')
    def run(*args, check=True, **kw):
        return sp.run(args, env=env, cwd=root, check=check, timeout=5, **kw)

    cookie = secrets.token_hex(16)
    (root/'auth').touch(mode=0o600)
    run('xauth', 'add', ':65535', '.', cookie)
    # The pane's program displays a line followed by CR, positioning the actual
    # terminal cursor at column zero for v/Enter. It records only PTY stdin.
    token = chr(0x4e00 + secrets.randbelow(0x4000))  # A fresh, single selectable glyph.
    fixture = root/'pane'
    fixture.write_text(f'''#!{bash}
set -eu
{shutil.which('stty')} -echo
printf '\\033[?2004l\\r%s\\r' '{token}'
printf '%s\\n' "$$ $(< /proc/self/comm)" > '{root}/ready'
IFS= read -r input
printf '%s' "$input" > '{root}/received.tmp'
{shutil.which('mv')} '{root}/received.tmp' '{root}/received'
exec {shutil.which('sleep')} 120
''')
    fixture.chmod(0o700)
    config = root/'config/config.toml'
    config.write_text(f'''[general]
default_shell = "{fixture}"
[behavior]
pane_shell_integration = false
[keybindings.modes.insert.bindings]
"ctrl+a [" = "enter_scroll_mode"
[keybindings.modes.scroll.bindings]
v = "begin_selection"
right = "move_cursor_right"
enter = "confirm_scrollback"
''')
    env['SHELL'] = str(fixture)
    with contextlib.ExitStack() as stack:
        start = lambda *a, **kw: stack.enter_context(process(*a, **kw))
        xvfb = start(['Xvfb', '-displayfd', '1', '-auth', str(root/'auth'), '-nolisten', 'tcp',
                      '-screen', '0', '1280x800x24'], env, root, 'xvfb.log', stdout=sp.PIPE)
        if not select.select([xvfb.stdout], [], [], 10)[0]:
            raise TimeoutError('private Xvfb')
        env['DISPLAY'] = ':' + str(int(xvfb.stdout.readline()))
        run('xauth', 'add', env['DISPLAY'], '.', cookie)
        product = dict(env, PATH=str(root/'bin'))
        cli = [str(binaries/'bmux'), '--runtime', 'default', '--target', 'local', '--config', str(config)]
        server = start([*cli, 'server', 'start'], product, root, 'bmux.log')
        def alive(proc):
            if proc.poll() is not None:
                raise RuntimeError(f'{proc.args[0]} exited {proc.returncode}; see {root}')
            return True
        wait_for('owned server socket', lambda: alive(server) and (root/'runtime/server.sock').is_socket())
        app = start([binaries/'bterm', '--runtime-root', root, '--bmux-config', config],
                    {k: v for k, v in product.items() if not k.startswith('BMUX_')}, root, 'bterm.log')
        wait_for('target pane started', lambda: alive(app) and (root/'ready').exists())
        def window():
            alive(app)
            found = run('xdotool', 'search', '--onlyvisible', '--pid', str(app.pid),
                        check=False, stdout=sp.PIPE).stdout.split()
            return found[0] if len(found) == 1 else None
        win = wait_for('one mapped Bterm window', window)
        run('xdotool', 'windowfocus', '--sync', win)
        peer = ['xclip', '-selection', 'clipboard', '-target', 'UTF8_STRING']
        def keys(*names):
            run('xdotool', 'key', '--clearmodifiers', '--delay', '90', *names)
        # Wait for presentation before acquiring an anchored selection, not just for map.
        time.sleep(1)
        keys('ctrl+a', 'bracketleft')
        time.sleep(.5)
        keys('v', 'Return')
        def pasted(value):
            result = run(*peer, '-out', check=False, stdout=sp.PIPE, stderr=sp.PIPE)
            return result.returncode == 0 and result.stdout == value
        wait_for('scroll copy -> independent xclip exact bytes', lambda: pasted(token.encode()))
        (root/'copied').write_bytes(token.encode())
        print('PASS scroll selection -> xclip', hashlib.sha256(token.encode()).hexdigest(), flush=True)
        # Replace CLIPBOARD with independent data the pane has never displayed or read.
        # Only Ctrl+Shift+V and Return enter the window; never type the paste value.
        external = ('Peer-' + secrets.token_hex(12)).encode()
        with (root/'peer-input').open('wb') as out:
            out.write(external)
        with (root/'peer-input').open('rb') as data:
            owner = start([*peer, '-in', '-quiet'], env, root, 'xclip.log', stdin=data)
        wait_for('external ownership', lambda: alive(owner) and pasted(external))
        if (root/'received').exists():
            raise AssertionError('pane already received a line before paste')
        keys('ctrl+shift+v')
        time.sleep(.3)
        keys('Return')
        wait_for('focused pane receipt', lambda: alive(app) and (root/'received').is_file())
        got = (root/'received').read_bytes()
        if got != external:
            raise AssertionError(f'target pane received {got!r}, wanted {external!r}')
        print('PASS xclip -> Bterm Ctrl+Shift+V -> target pane', hashlib.sha256(got).hexdigest(), flush=True)


if __name__ == '__main__':
    main()
