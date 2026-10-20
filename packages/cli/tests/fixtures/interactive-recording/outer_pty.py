"""Owned real outer-terminal driver. Never opens a user terminal/runtime."""
import errno
import fcntl
import json
import os
import re
import selectors
import socket
import struct
import subprocess
import sys
import termios
import threading
import time

# A session owner must outlive BMUX: macOS revokes the slave when its
# controlling-session leader exits, even with the driver's observer fd open.
if sys.argv[1] == '--terminal-owner':
    owner_root = sys.argv[2]
    before = termios.tcgetattr(0)
    product = subprocess.Popen(sys.argv[3:], close_fds=True)
    try:
        code = product.wait(timeout=120)
        after = termios.tcgetattr(0)
        with open(owner_root + '/owner-report.json', 'w') as output:
            json.dump({'exit_code': code, 'restored': before == after}, output)
        # Prove the same owned terminal is still usable, before owner retirement.
        os.write(1, b'\r\nOUTER_OWNER_RESTORATION_CHECKED\r\n')
    finally:
        if product.poll() is None:
            product.kill()  # exact child handle, never a process-tree sweep
            product.wait(timeout=5)
    sys.exit(0)

def visible_green_marker(output):
    # Decode SGR on the actual rendered outer-terminal stream, not source raw
    # events. Accept ANSI, indexed, and RGB representations of green background.
    green = False
    text = bytearray()
    position = 0
    for escape in re.finditer(rb'\x1b\[[0-?]*[ -/]*[@-~]', output):
        chunk = output[position:escape.start()]
        if green:
            text.extend(chunk)
            if b'FINALMARKER' in text:
                return True
        else:
            text.clear()
        if escape.group().endswith(b'm'):
            parameters = escape.group()[2:-1].split(b';')
            values = [int(value) if value else 0 for value in parameters]
            index = 0
            while index < len(values):
                value = values[index]
                if value in (0, 49):
                    green = False
                elif 40 <= value <= 47 or 100 <= value <= 107:
                    green = value in (42, 102)
                elif value in (38, 48) and index + 1 < len(values):
                    color_mode = values[index + 1]
                    if color_mode == 5 and index + 2 < len(values):
                        if value == 48:
                            green = values[index + 2] in (2, 10)
                        index += 2
                    elif color_mode == 2 and index + 4 < len(values):
                        if value == 48:
                            red, shade, blue = values[index + 2:index + 5]
                            green = shade > 80 and shade > red * 2 and shade > blue * 2
                        index += 4
                index += 1
        position = escape.end()
    if green:
        text.extend(output[position:])
    return b'FINALMARKER' in text

root, binary, mode, fixture = sys.argv[1:]
if mode == 'no-tty':
    command = [binary, '--runtime', 'default', '--target', 'local', '--config',
               root + '/config/bmux.toml', 'record', '--output', root + '/exports',
               '--json', '--', '/bin/sh', '-c', 'printf launched > forbidden-launch']
    result = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
    with open(root + '/outer-report.json', 'w') as output:
        json.dump({'exit_code': result.returncode}, output)
    with open(root + '/outer-transcript.bin', 'wb') as output:
        output.write(result.stdout + result.stderr)
    sys.exit(0)
master, slave = os.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))

def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)

args = [binary, '--runtime', 'default', '--target', 'local', '--config',
        root + '/config/bmux.toml', 'record', '--output', root + '/exports',
        '--export', 'raw', '--export', 'text', '--export', 'events', '--json']
if mode == 'gif':
    args += ['--gif', root + '/animation.gif']
args += ['--', sys.executable, fixture, mode, root]
if mode == 'launch-error':
    args = args[:-4] + [root + '/missing-executable']
child = subprocess.Popen([sys.executable, __file__, '--terminal-owner', root] + args,
                         stdin=slave, stdout=slave, stderr=slave,
                         close_fds=True, preexec_fn=controlling_terminal)
notify, sender = socket.socketpair()

def wait_child():
    child.wait()
    sender.send(b'x')

waiter = threading.Thread(target=wait_child)
waiter.start()
selector = selectors.DefaultSelector()
selector.register(master, selectors.EVENT_READ)
selector.register(notify, selectors.EVENT_READ)
os.set_blocking(master, False)
transcript = bytearray()
resized = False
sent = False
acknowledged = False
exited = False
# Pending capture deliberately exhausts the product's 30-second post-exit
# cleanup wait, after startup/rendering. The driver must outlive that contract.
deadline = time.monotonic() + (120 if mode == 'gif' else 60 if mode == 'pending-capture' else 30)
try:
    while not exited:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('interactive record deadline exceeded')
        for key, _ in selector.select(remaining):
            if key.fileobj is notify:
                notify.recv(1)
                exited = True
                continue
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    selector.unregister(master)
                    continue
                raise
            if not data:
                selector.unregister(master)
                continue
            transcript.extend(data)
            if len(transcript) > 8 * 1024 * 1024:
                raise RuntimeError('outer terminal evidence exceeded bound')
            if b'READY' in transcript and not resized:
                fcntl.ioctl(slave, termios.TIOCSWINSZ,
                            struct.pack('HHHH', 31, 93, 0, 0))
                resized = True
            if b'RESIZE:93x31' in transcript and not sent:
                os.write(master, b'\x03' if mode == 'ctrl-c' else 'quit héllo\n'.encode())
                sent = True
            if sent and not acknowledged and visible_green_marker(transcript):
                os.write(master, b'!')
                acknowledged = True
    # Child completion is distinct from EOF (we own an observer slave).
    # Drain all kernel-queued outer bytes after wait, without waiting for fake EOF.
    while True:
        try:
            data = os.read(master, 65536)
        except BlockingIOError:
            break
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not data:
            break
        transcript.extend(data)
    if child.returncode != 0:
        raise RuntimeError('outer terminal owner failed')
    with open(root + '/owner-report.json') as source:
        report = json.load(source)
    report.update({'resized': resized, 'sent_input': sent,
                   'visible_green_acknowledged': acknowledged})
    with open(root + '/outer-report.json', 'w') as output:
        json.dump(report, output)
finally:
    with open(root + '/outer-transcript.bin', 'wb') as output:
        output.write(transcript)
    if child.poll() is None:
        # Only the direct subprocess handle created by this driver; no tree sweep.
        child.kill()
    waiter.join(timeout=5)
    selector.close()
    notify.close()
    sender.close()
    os.close(master)
    os.close(slave)
