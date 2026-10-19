#!/usr/bin/env python3
"""Isolated installed-artifact crash matrix (never connects to a BMUX server).

Usage: python3 .../supervisor_crash_tests.py /absolute/test/execution-supervisor
The caller builds/selects the test artifact; no build or production discovery here.
"""
import hashlib
import json
import os
from pathlib import Path
import pty
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import threading
import uuid

VERSION = 6
TOKEN = "isolated-crash-test"
AUTH = "private-fixture"


def frame(sock, value):
    data = json.dumps(value).encode()
    sock.sendall(struct.pack(">I", len(data)) + data)


def receive(sock):
    def exact(n):
        data = b""
        while len(data) < n:
            part = sock.recv(n - len(data))
            if not part:
                raise EOFError("helper closed frame")
            data += part
        return data
    n = struct.unpack(">I", exact(4))[0]
    assert n <= 262144
    return json.loads(exact(n))


def until(fn):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        value = fn()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError("isolated helper deadline exceeded")


class Execution:
    def __init__(self, artifact, program, args, drain_output=True):
        self.root = Path(tempfile.mkdtemp(prefix="bmux-proof-", dir="/tmp"))
        os.chmod(self.root, 0o700)
        self.master, self.slave = pty.openpty()
        # The server normally drains PTY output concurrently. Without a reader,
        # tcdrain is intentionally not an acknowledged capture boundary.
        def drain():
            try:
                while os.read(self.master, 65536):
                    pass
            except OSError:
                pass
        self.reader = threading.Thread(target=drain, daemon=True)
        if drain_output:
            self.reader.start()
        bootstrap = self.root / "bootstrap.json"
        bootstrap.write_text(json.dumps(dict(version=VERSION, execution_token=TOKEN, authentication=AUTH)))
        os.chmod(bootstrap, 0o600)
        self.lease, peer = socket.socketpair()
        # Use a private shell only to map the explicitly inherited descriptor to
        # fd 3; exec replaces it. No production processes or endpoints touched.
        fd = peer.fileno()
        self.guardian = subprocess.Popen(
            ["/bin/sh", "-c", f'exec 3<&{fd}; ' + (f'exec {fd}<&-; ' if fd != 3 else '') + 'exec "$@"', "fixture", str(artifact),
             "--guardian-terminal-session-v4", str(bootstrap), str(self.root / "control.sock"), os.ttyname(self.slave)],
            pass_fds=(fd,), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        peer.close()
        self.lease.settimeout(10)
        assert receive(self.lease) == VERSION
        self.request(dict(operation="start", mutation_id=str(uuid.uuid4()), program=program, args=args,
                          cwd=str(self.root), env=dict(os.environ)))
        self.target = until(lambda: self.status().get("target_pid"))

    def request(self, operation):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(10)
            sock.connect(str(self.root / "control.sock"))
            frame(sock, dict(version=VERSION, execution_token=TOKEN, authentication=AUTH, operation=operation))
            response = receive(sock)
            if response.get("error") and operation["operation"] == "start":
                raise AssertionError(f"launch rejected: {response}")
            return response

    def status(self):
        return self.request(dict(operation="status"))

    def owner(self):
        # Diagnostic discovery only; this PID is signaled only after its direct
        # child relationship to our live guardian is verified.
        result = subprocess.check_output(["/bin/ps", "-axo", "pid=,ppid="], text=True)
        return until(lambda: next((int(line.split()[0]) for line in result.splitlines()
                                  if len(line.split()) == 2 and int(line.split()[1]) == self.guardian.pid), None))

    def close(self):
        self.lease.close()
        if self.guardian.poll() is None:
            self.guardian.wait(timeout=15)
        os.close(self.master)
        os.close(self.slave)
        shutil.rmtree(self.root)


def gone(pid):
    result = subprocess.run(["/bin/ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True, check=False)
    return result.returncode != 0 or result.stdout.strip().startswith("Z")


def run_case(artifact, case, program="/bin/sh", args=None):
    execution = Execution(artifact, program, args or ["-c", "trap '' HUP TERM; echo $$ > target; while :; do sleep 1; done"])
    try:
        target = execution.target
        if case == "lease":
            execution.lease.close()
            execution.guardian.wait(timeout=15)
        elif case == "server-kill":
            # A fresh isolated lease-owning process stands in for the server.
            # Parent deliberately relinquishes its copy before killing it.
            owner = subprocess.Popen(
                [sys.executable, "-c", "import os,sys; fd=int(sys.argv[1]); os.set_blocking(fd,True); os.write(1,b'ready\\n'); os.read(fd,1)", str(execution.lease.fileno())],
                pass_fds=(execution.lease.fileno(),), stdout=subprocess.PIPE,
            )
            execution.lease.close()
            assert owner.stdout.readline() == b"ready\n"
            owner.kill()
            assert owner.wait(timeout=5) == -signal.SIGKILL
            execution.guardian.wait(timeout=15)
        elif case == "guardian":
            execution.guardian.kill()  # owned direct Child, not a discovered PID
            execution.guardian.wait(timeout=5)
        elif case == "owner":
            owner = execution.owner()
            assert os.getsid(owner) == execution.guardian.pid
            os.kill(owner, signal.SIGKILL)
            status = until(lambda: (s if (s := execution.status()).get("terminal_session_ended") else None))
            assert status.get("exit_code") is None and status.get("exit_signal") is None, status
            assert status.get("error"), status  # helper loss is NOT target exit
        elif case == "natural":
            status = until(lambda: (s if (s := execution.status()).get("terminal_session_ended") else None))
            assert status.get("exit_code") == 23 and status.get("exit_signal") is None, status
            assert not status.get("error") and not status.get("cleanup_error"), status
            response = execution.request(dict(operation="retire_owner", mutation_id=str(uuid.uuid4())))
            assert not response.get("error"), response
            execution.guardian.wait(timeout=5)
        elif case == "retire":
            status = execution.request(dict(operation="sweep", mutation_id=str(uuid.uuid4())))
            assert status.get("exit_signal") == signal.SIGKILL, status
            response = execution.request(dict(operation="retire_owner", mutation_id=str(uuid.uuid4())))
            assert not response.get("error"), response
            execution.guardian.wait(timeout=5)
        until(lambda: gone(target))
        print(f"PASS {case} {program}", flush=True)
    finally:
        execution.close()


def main():
    artifact = Path(sys.argv[1]).resolve(strict=True)
    print(f"test artifact: {artifact} sha256={hashlib.sha256(artifact.read_bytes()).hexdigest()}", flush=True)
    for case in ("lease", "server-kill", "guardian", "owner", "retire"):
        run_case(artifact, case)
    run_case(artifact, "natural", "/bin/sh", ["-c", "printf 'final output\\n'; exit 23"])
    # Target must not inherit the owner's ignored job-control dispositions.
    # TOSTOP background write must stop its own child with SIGTTOU.
    job_control = """import os,signal,termios
for sig in [signal.SIGTTOU,signal.SIGTTIN,signal.SIGTSTP,signal.SIGCHLD,signal.SIGALRM]:
    assert signal.getsignal(sig) != signal.SIG_IGN, sig
assert not signal.pthread_sigmask(signal.SIG_BLOCK, [])
attrs=termios.tcgetattr(0)
attrs[3] |= termios.TOSTOP
termios.tcsetattr(0, termios.TCSANOW, attrs)
pid=os.fork()
if pid == 0:
    os.setpgid(0,0)
    os.write(1,b'background write')
    os._exit(99)
_,status=os.waitpid(pid,os.WUNTRACED)
assert os.WIFSTOPPED(status) and os.WSTOPSIG(status)==signal.SIGTTOU, status
os.kill(pid,signal.SIGKILL)
os.waitpid(pid,0)
os._exit(23)
"""
    run_case(artifact, "natural", sys.executable, ["-c", job_control])
    # Interactive Bash guarantees a distinct job-control background group.
    jobs = ["--noprofile", "--norc", "-imc", "trap '' HUP TERM; (trap '' HUP TERM; while :; do sleep 1; done) & echo $! > background; wait"]
    for case in ("lease", "guardian", "owner"):
        execution = Execution(artifact, "/bin/bash", jobs)
        try:
            background = until(lambda: int((execution.root / "background").read_text())
                               if (execution.root / "background").exists() else None)
            assert os.getpgid(background) != os.getpgid(execution.target)
            if case == "lease":
                execution.lease.close()
                execution.guardian.wait(timeout=15)
            elif case == "guardian":
                execution.guardian.kill()
                execution.guardian.wait(timeout=5)
            else:
                os.kill(execution.owner(), signal.SIGKILL)
            until(lambda: gone(background) and gone(execution.target))
            print(f"PASS {case} background job group", flush=True)
        finally:
            execution.close()
    for case in ("sweep", "lease"):
        execution = Execution(artifact, "/bin/sh", ["-c", "printf x; while :; do sleep 1; done"], drain_output=False)
        try:
            # Ensure output reached the slave queue before cleanup. Do not read
            # the master: this deliberately models a failed server PTY reader.
            import fcntl
            import termios
            if sys.platform == "darwin":
                until(lambda: struct.unpack("i", fcntl.ioctl(execution.slave, termios.TIOCOUTQ, struct.pack("i", 0)))[0] > 0)
            owner = execution.owner()
            start = time.monotonic()
            if case == "sweep":
                response = execution.request(dict(operation="sweep", mutation_id=str(uuid.uuid4())))
                state = until(lambda: (s if (s := execution.status()).get("terminal_session_ended") else None))
                assert state.get("exit_signal") == signal.SIGKILL, state
                if sys.platform == "darwin":
                    assert "queued bytes discarded" in state.get("capture_error", ""), state
                    assert not response.get("error") and not response.get("cleanup_error"), response
                    assert response.get("target_terminated"), response
                    retired = execution.request(dict(operation="retire_owner", mutation_id=str(uuid.uuid4())))
                    assert not retired.get("error") and not retired.get("cleanup_error"), retired
                    assert retired.get("capture_error") == state.get("capture_error"), retired
            execution.lease.close()
            execution.guardian.wait(timeout=3)
            assert time.monotonic() - start < 3
            until(lambda: gone(execution.target) and gone(owner))
            outcome = json.loads((execution.root / "terminal-owner-outcome.json").read_text())
            assert outcome["exit_signal"] == signal.SIGKILL, outcome
            if sys.platform == "darwin":
                assert "queued bytes discarded" in outcome["drain_error"], outcome
                assert execution.guardian.returncode == 0
            print(f"PASS no-reader {case}: bounded retirement, capture failure preserved", flush=True)
        finally:
            execution.close()
    nu = shutil.which("nu")
    if nu:
        run_case(artifact, "lease", nu, ["--no-config-file", "-c", "loop { sleep 1sec }"])
    else:
        print("SKIP Nushell: executable unavailable")


if __name__ == "__main__":
    main()
