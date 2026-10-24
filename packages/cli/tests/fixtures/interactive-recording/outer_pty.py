"""Owned real outer-terminal driver. Never opens a user terminal/runtime."""
import errno
import hashlib
import math
import resource
import tempfile
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


# This opt-in acceptance mode shares the real terminal owner below. It never
# connects to an existing runtime and deliberately emits only synthetic tokens.
LIMIT = 8 * 1024 * 1024


def latency_fixture(root, count):
    import tty
    assert all(os.isatty(fd) for fd in (0, 1, 2))
    before = termios.tcgetattr(0)
    samples = []
    tty.setraw(0)
    try:
        os.write(1, b'LATENCY_BYTE0\x1b[?1049h\x1b[2J\x1b[HLATENCY_READY')
        for number in range(1, count + 1):
            if os.read(0, 1) != b'x':
                raise RuntimeError('fixture expected synthetic sample input')
            received = time.monotonic_ns()
            token = ('ECHO%06d' % number).encode()
            before_write = time.monotonic_ns()
            os.write(1, b'\x1b[2;1H' + token)
            samples.append({'sample_id': number, 'pty_read_ns': received,
                            'before_echo_write_ns': before_write,
                            'after_echo_write_ns': time.monotonic_ns()})
        # Driver acknowledges the final displayed sample before target exit.
        if os.read(0, 1) != b'!':
            raise RuntimeError('missing final displayed-echo acknowledgement')
        # A single metadata write after measurement, never on the per-key path.
        with open(root + '/fixture-samples.json', 'x') as output:
            json.dump(samples, output)
        os.write(1, ('\x1b[3;1HLATENCY_TAIL%06d\x1b[?1049l' % count).encode())
    finally:
        # All synthetic inputs (including final acknowledgement) are consumed.
        # On macOS TCSANOW when restoring ICANON leaves kernel PENDIN set;
        # TCSAFLUSH clears that pending-input state and restores exact attrs.
        # This fixture-owned cleanup must not mask product restoration: the
        # outer owner still compares every tcgetattr field without normalization.
        termios.tcsetattr(0, termios.TCSAFLUSH, before)


class LatencyScreen:
    """Bounded ASCII fixture observer, not a general terminal emulator.

    As in the retained-renderer acceptance test, match reconstructed rows so
    suffix-only paints cannot masquerade as missing or complete whole tokens.
    """
    def __init__(self):
        self.rows = [bytearray(b' ' * 80) for _ in range(24)]
        self.x = self.y = 0
        self.saved = (0, 0)
        self.pending = b''
        self.last = 32

    def put(self, byte):
        if self.x >= 80:
            self.x = 0
            self.y = min(23, self.y + 1)
        self.rows[self.y][self.x] = byte
        self.x += 1
        self.last = byte

    def feed(self, data):
        data = self.pending + data
        i = 0
        while i < len(data):
            byte = data[i]
            if byte == 27:
                if i + 1 == len(data):
                    break
                if data[i + 1] == 91:
                    match = re.match(rb'\x1b\[([0-?]*)([ -/]*)([@-~])', data[i:])
                    if not match:
                        break
                    params, _, command = match.groups()
                    p = [int(v or b'0') for v in params.split(b';')] if re.fullmatch(rb'[0-9;]*', params) else []
                    n = (p[0] or 1) if p else 1
                    if command in (b'H', b'f'):
                        self.y = min(23, max(0, (p[0] or 1) - 1)) if p else 0
                        self.x = min(79, max(0, (p[1] or 1) - 1)) if len(p) > 1 else 0
                    elif command == b'G': self.x = min(79, n - 1)
                    elif command == b'd': self.y = min(23, n - 1)
                    elif command == b'A': self.y = max(0, self.y - n)
                    elif command == b'B': self.y = min(23, self.y + n)
                    elif command == b'C': self.x = min(79, self.x + n)
                    elif command == b'D': self.x = max(0, self.x - n)
                    elif command == b'E': self.y, self.x = min(23, self.y + n), 0
                    elif command == b'F': self.y, self.x = max(0, self.y - n), 0
                    elif command == b'b':
                        if n > 80 * 24:
                            raise RuntimeError('repeat exceeds fixture geometry')
                        for _ in range(n): self.put(self.last)
                    elif command == b'K':
                        mode = p[0] if p else 0
                        start, end = (0, 80) if mode == 2 else ((0, min(80, self.x + 1)) if mode == 1 else (self.x, 80))
                        self.rows[self.y][start:end] = b' ' * (end - start)
                    elif command == b'J' and p and p[0] in (2, 3):
                        self.rows = [bytearray(b' ' * 80) for _ in range(24)]
                    elif command == b's': self.saved = (self.x, self.y)
                    elif command == b'u': self.x, self.y = self.saved
                    i += len(match.group())
                    continue
                if data[i + 1] == 93:
                    match = re.match(rb'\x1b\].*?(?:\x07|\x1b\\)', data[i:], re.S)
                    if not match: break
                    i += len(match.group())
                    continue
                if data[i + 1] == 55: self.saved = (self.x, self.y)
                if data[i + 1] == 56: self.x, self.y = self.saved
                i += 2
                continue
            if byte == 13: self.x = 0
            elif byte == 10: self.y = min(23, self.y + 1)
            elif byte == 8: self.x = max(0, self.x - 1)
            elif 32 <= byte < 127: self.put(byte)
            i += 1
        self.pending = data[i:]
        if len(self.pending) >= 65536:
            raise RuntimeError('incomplete terminal escape exceeded bound')

    def contains(self, token):
        return any(token in row for row in self.rows)


def digest_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def latency_acceptance(arguments):
    # Explicit existing binary, new evidence directory, separate short socket
    # parent, direct/recorded path. No implicit PATH executable or runtime reuse.
    binary, root, socket_parent, mode, count_string = arguments
    if mode not in ('direct', 'recorded'):
        raise ValueError('latency mode must be direct or recorded')
    count = int(count_string)
    if not 20 <= count <= 1000:
        raise ValueError('sample count must be 20..1000')
    if not os.path.isabs(binary):
        raise ValueError('binary must be an explicit absolute path')
    binary = os.path.realpath(binary)
    if not os.path.isabs(root) or not os.path.isabs(socket_parent):
        raise ValueError('evidence and socket parent must be absolute')
    if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        raise ValueError('explicit binary is not executable')
    os.mkdir(root, 0o700)  # exclusive: never adopt an existing runtime/evidence root
    root = os.path.realpath(root)
    short = tempfile.mkdtemp(prefix='il-', dir=os.path.realpath(socket_parent))
    if len(os.fsencode(short)) > 25:
        raise ValueError('socket directory exceeds endpoint budget; use /tmp')
    env = {key: value for key, value in os.environ.items() if not key.startswith('BMUX_')}
    for key, directory in [('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'),
                           ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state'),
                           ('XDG_CACHE_HOME', 'cache'), ('XDG_RUNTIME_DIR', 'runtime'),
                           ('BMUX_CONFIG_DIR', 'config'), ('BMUX_DATA_DIR', 'data'),
                           ('BMUX_STATE_DIR', 'state'), ('BMUX_RUNTIME_DIR', 'runtime'),
                           ('BMUX_LOG_DIR', 'logs'), ('BMUX_RECORDINGS_DIR', 'recordings')]:
        os.makedirs(root + '/' + directory, mode=0o700, exist_ok=True)
        env[key] = root + '/' + directory
    env.update(TMPDIR=short, BMUX_NO_BASE_CONFIG='1', BMUX_RUNTIME_NAME='default',
               BMUX_TARGET='local', TERM='xterm-256color')
    script = os.path.realpath(__file__)
    args = [sys.executable, script, '--latency-child', root, str(count)]
    if mode == 'recorded':
        args = [binary, '--runtime', 'default', '--target', 'local', 'record',
                '--output', root + '/exports', '--export', 'raw', '--json', '--'] + args
    report = {'schema_version': 1, 'passed': False, 'mode': mode, 'samples': [],
              'binary': binary, 'binary_sha256': digest_file(binary),
              'geometry': [80, 24], 'socket_root': short, 'evidence_root': root,
              'clock': 'Python monotonic_ns, same host',
              'durable_capture_verified': False, 'human_stream_negotiation_verified': False}
    transcript = bytearray()
    screen = LatencyScreen()
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
    def terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    child = subprocess.Popen([sys.executable, script, '--terminal-owner', root] + args,
                             stdin=slave, stdout=slave, stderr=slave, env=env,
                             cwd=root, close_fds=True, preexec_fn=terminal)
    os.set_blocking(master, False)
    notify, sender = socket.socketpair()
    def waiter():
        child.wait()
        sender.send(b'x')
    thread = threading.Thread(target=waiter, daemon=True)
    thread.start()
    selector = selectors.DefaultSelector()
    selector.register(master, selectors.EVENT_READ)
    selector.register(notify, selectors.EVENT_READ)
    exited = False
    started = time.monotonic()
    deadline = started + 110
    cpu_start = time.process_time()

    def capture(data):
        if len(transcript) + len(data) > LIMIT:
            raise RuntimeError('latency transcript exceeded 8 MiB bound')
        transcript.extend(data)
        screen.feed(data)

    def pump(until):
        nonlocal exited
        remaining = min(deadline, until) - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('latency fixture deadline exceeded')
        for key, _ in selector.select(remaining):
            if key.fileobj is notify:
                notify.recv(1)
                selector.unregister(notify)
                exited = True
            else:
                try:
                    data = os.read(master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO: raise
                    data = b''
                if data:
                    capture(data)
                else:
                    selector.unregister(master)

    def await_screen(token, seconds):
        until = time.monotonic() + seconds
        while not screen.contains(token):
            if exited:
                raise RuntimeError('terminal owner exited before expected frame')
            pump(until)
        return time.monotonic_ns()

    try:
        await_screen(b'LATENCY_READY', 60)  # startup is not interactive latency
        measurement_start = time.monotonic_ns()
        for number in range(1, count + 1):
            sent = time.monotonic_ns()
            if os.write(master, b'x') != 1:
                raise RuntimeError('short synthetic input write')
            displayed = await_screen(('ECHO%06d' % number).encode(), 5)
            report['samples'].append({'sample_id': number, 'sent_ns': sent,
                                      'displayed_echo_ns': displayed})
        report['interactive_wall_seconds'] = (time.monotonic_ns() - measurement_start) / 1e9
        os.write(master, b'!')
        while not exited:
            pump(deadline)
        # The observer slave intentionally prevents EOF. Owner completion means
        # no more writers; drain every kernel-queued byte, still under the bound.
        while True:
            try:
                data = os.read(master, 65536)
            except BlockingIOError:
                break
            except OSError as error:
                if error.errno == errno.EIO: break
                raise
            if not data: break
            capture(data)
        report['finalization_observed_after_last_echo_ms'] = (
            time.monotonic_ns() - report['samples'][-1]['displayed_echo_ns']) / 1e6
        if child.returncode != 0:
            raise RuntimeError('terminal owner failed')
        with open(root + '/owner-report.json') as source:
            owner = json.load(source)
        report['owner'] = owner
        if owner['exit_code'] != 0 or not owner['restored']:
            raise RuntimeError('product failed or outer terminal was not restored')
        if b'OUTER_OWNER_RESTORATION_CHECKED' not in transcript:
            raise RuntimeError('missing complete outer-owner tail')
        if mode == 'direct' and ('LATENCY_TAIL%06d' % count).encode() not in transcript:
            raise RuntimeError('missing direct fixture final tail')
        with open(root + '/fixture-samples.json') as source:
            received = json.load(source)
        if len(received) != count:
            raise RuntimeError('missing fixture receipt samples')
        for number, (sample, receipt) in enumerate(zip(report['samples'], received), 1):
            if sample['sample_id'] != number or receipt['sample_id'] != number:
                raise RuntimeError('sample identity mismatch')
            sample.update(receipt)
            if not (sample['sent_ns'] <= sample['pty_read_ns'] <= sample['before_echo_write_ns']
                    <= sample['after_echo_write_ns'] and sample['before_echo_write_ns'] <= sample['displayed_echo_ns']):
                raise RuntimeError('sample clock/order mismatch')
            sample['pty_receipt_ms'] = (sample['pty_read_ns'] - sample['sent_ns']) / 1e6
            sample['displayed_echo_ms'] = (sample['displayed_echo_ns'] - sample['sent_ns']) / 1e6
        if mode == 'recorded':
            results = []
            for line in bytes(transcript).splitlines():
                start = line.find(b'{')
                if start >= 0:
                    try: results.append(json.loads(line[start:]))
                    except ValueError: pass
            final = next((value for value in reversed(results) if value.get('schema_version') == 4), None)
            if final is None:
                raise RuntimeError('missing structured finalization result')
            status = final['status']
            for field in ('target_exited', 'terminal_session_ended', 'capture_eof', 'output_drained',
                          'capture_finalized', 'durable_recording_complete', 'timing_complete'):
                if status.get(field) is not True:
                    raise RuntimeError('incomplete finalization: ' + field)
            if status.get('exit_code') != 0 or status.get('recovery_required') is not False or status.get('errors') != []:
                raise RuntimeError('finalized status contains failure/recovery')
            final_start = None
            for match in re.finditer(rb'[^\n]*\n|[^\n]+$', bytes(transcript)):
                brace = match.group().find(b'{')
                if brace >= 0:
                    try:
                        value = json.loads(match.group()[brace:])
                        if value.get('schema_version') == 4:
                            final_start = match.start() + brace
                    except ValueError:
                        pass
            if final_start is None or transcript.rfind(b'\x1b[?1049l', 0, final_start) < 0:
                raise RuntimeError('missing terminal restoration before result')
            raw_path = root + '/exports/output.pty'
            if os.path.getsize(raw_path) > LIMIT:
                raise RuntimeError('raw capture exceeded fixture bound')
            with open(raw_path, 'rb') as source:
                raw = source.read(LIMIT + 1)
            expected = b'LATENCY_BYTE0\x1b[?1049h\x1b[2J\x1b[HLATENCY_READY'
            for number in range(1, count + 1):
                expected += ('\x1b[2;1HECHO%06d' % number).encode()
            expected += ('\x1b[3;1HLATENCY_TAIL%06d\x1b[?1049l' % count).encode()
            if raw != expected:
                raise RuntimeError('durable capture differs from complete numbered fixture stream')
            for field in ('output_watermark', 'capture_watermark', 'recording_watermark'):
                if status.get(field) != len(raw):
                    raise RuntimeError('durable watermark mismatch: ' + field)
            report['durable_capture_verified'] = True
            report['durable_capture_bytes'] = len(raw)
            report['durable_capture_sha256'] = hashlib.sha256(raw).hexdigest()
        for metric in ('pty_receipt_ms', 'displayed_echo_ms'):
            values = sorted(sample[metric] for sample in report['samples'])
            report[metric] = {'p50': values[math.ceil(len(values) * .50) - 1],
                              'p95': values[math.ceil(len(values) * .95) - 1], 'max': values[-1]}
        report['passed'] = True  # completeness, not an implicit latency budget
    except Exception as error:
        report['failure'] = type(error).__name__ + ': ' + str(error)
        raise
    finally:
        report['driver_cpu_seconds'] = time.process_time() - cpu_start
        report['wall_seconds'] = time.monotonic() - started
        report['outer_transcript_bytes'] = len(transcript)
        report['cpu_scope'] = 'driver and owner-reaped product children; detached runtime CPU excluded'
        with open(root + '/latency-report.json', 'w') as output:
            json.dump(report, output, indent=2)
        with open(root + '/outer-transcript.bin', 'wb') as output:
            output.write(transcript)
        if child.poll() is None:
            child.kill()  # exact owned owner only; uncertain isolated state retained
        thread.join(timeout=5)
        selector.close()
        notify.close()
        sender.close()
        os.close(master)
        os.close(slave)
    print(json.dumps({'report': root + '/latency-report.json', 'passed': report['passed']}))


if sys.argv[1] == '--latency-child':
    latency_fixture(sys.argv[2], int(sys.argv[3]))
    sys.exit(0)
if sys.argv[1] == '--latency':
    latency_acceptance(sys.argv[2:])
    sys.exit(0)

# A session owner must outlive BMUX: macOS revokes the slave when its
# controlling-session leader exits, even with the driver's observer fd open.
if sys.argv[1] == '--terminal-owner':
    owner_root = sys.argv[2]
    before = termios.tcgetattr(0)
    usage_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    owner_started = time.monotonic()
    product = subprocess.Popen(sys.argv[3:], close_fds=True)
    try:
        code = product.wait(timeout=120)
        after = termios.tcgetattr(0)
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
        # Numeric-only bounded diagnostics; keep exact tcgetattr equality as
        # the restoration gate (do not normalize away a real mode difference).
        differences = []
        for index, (old, new) in enumerate(zip(before, after)):
            if index == 6:
                for cc, (a, b) in enumerate(zip(old, new)):
                    if a != b:
                        differences.append({'field': index, 'cc_index': cc,
                                            'before': a if isinstance(a, int) else a[0],
                                            'after': b if isinstance(b, int) else b[0]})
            elif old != new:
                differences.append({'field': index, 'before': old, 'after': new})
        with open(owner_root + '/owner-report.json', 'w') as output:
            json.dump({'exit_code': code, 'restored': before == after,
                       'termios_differences': differences,
                       'reaped_children_user_seconds': usage.ru_utime - usage_before.ru_utime,
                       'reaped_children_system_seconds': usage.ru_stime - usage_before.ru_stime,
                       'owner_wall_seconds': time.monotonic() - owner_started}, output)
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
