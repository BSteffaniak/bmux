"""Real native terminal fixture, not a mocked BMUX transport."""
import os
import signal
import sys
import termios
import tty
import time

mode = sys.argv[1]
assert all(os.isatty(fd) for fd in (0, 1, 2))
with open('/dev/tty', 'rb', buffering=0):
    assert os.tcgetpgrp(0) == os.getpgrp()
before = termios.tcgetattr(0)
tty.setraw(0)
attrs = termios.tcgetattr(1)
attrs[1] &= ~termios.OPOST
termios.tcsetattr(1, termios.TCSANOW, attrs)

def emit(value):
    os.write(1, value)

def resized(_signal, _frame):
    size = os.get_terminal_size(0)
    emit(('RESIZE:%dx%d\n' % (size.columns, size.lines)).encode())

signal.signal(signal.SIGWINCH, resized)
emit(b'BYTE0\x1b[?1049h\x1b[2J\x1b[H\x1b[41mREADY\x1b[0m\n')
received = bytearray()
try:
    while True:
        byte = os.read(0, 1)
        if not byte:
            raise RuntimeError('unexpected terminal EOF')
        if byte == b'\x03':
            emit(b'CTRL_C\n')
            break
        received += byte
        if byte == b'\n':
            emit(b'KEY:' + bytes(received))
            if received == 'quit héllo\n'.encode():
                break
            received.clear()
    emit(b'\x1b[42mFINALMARKER\x1b[0m')
    # Only retire this displayed frame after the real outer terminal sees it.
    if os.read(0, 1) != b'!':
        raise RuntimeError('missing visible final-frame acknowledgment')
    if mode == 'gif':
        # Preserve the acknowledged source frame across GIF sampling intervals;
        # a fast retained renderer can otherwise retire it between samples.
        time.sleep(0.2)
    if mode == 'pending-capture':
        root = sys.argv[2]
        descendant = os.fork()
        if descendant == 0:
            signal.signal(signal.SIGHUP, signal.SIG_IGN)
            with open(root + '/slave-holder.pid', 'w') as output:
                output.write(str(os.getpid()))
            while not os.path.exists(root + '/release-slave'):
                time.sleep(0.01)
            for fd in (0, 1, 2):
                os.close(fd)
            os._exit(0)
        while not os.path.exists(root + '/slave-holder.pid'):
            time.sleep(0.01)
    emit(b'\x1b[?1049l')
finally:
    termios.tcsetattr(0, termios.TCSANOW, before)
sys.exit(7 if mode == 'nonzero' else 0)
