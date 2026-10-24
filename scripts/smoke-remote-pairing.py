#!/usr/bin/env python3
"""Exercise invitation pairing with an isolated host and daemonless client on Unix."""
import argparse
import errno
import hashlib
import os
from pathlib import Path
import pty
import re
import select
import struct
import termios
import fcntl
import subprocess
import tempfile
import time


class Terminal:
    def __init__(self, binary, env, args):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        self.process = subprocess.Popen(
            [str(binary), *args], env=env, stdin=slave, stdout=slave, stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        self.buffer = b""

    def expect(self, pattern, seconds=30):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            match = re.search(pattern, self.buffer)
            if match:
                self.buffer = self.buffer[match.end():]
                return match
            if select.select([self.master], [], [], 0.1)[0]:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b""
                if chunk:
                    self.buffer += chunk
                elif self.process.poll() is not None:
                    break
        # Bound and redact captured diagnostics before reporting a failed step.
        diagnostic = re.sub(rb"bmux-pair:v1:[0-9a-f]+", b"[invitation redacted]", self.buffer)
        diagnostic = re.sub(rb"[0-9a-fA-F]{32,}", b"[identifier redacted]", diagnostic)
        raise AssertionError(f"expected terminal step missing; exit={self.process.poll()}; output={diagnostic[:2800]!r}")

    def send(self, value):
        os.write(self.master, value)

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        os.close(self.master)


def environment(root):
    env = os.environ.copy()
    for name in ["CONFIG", "STATE", "RUNTIME", "DATA", "LOG"]:
        path = root / name.lower()
        path.mkdir(parents=True)
        env[f"BMUX_{name}_DIR"] = str(path)
    env.pop("BMUX_TARGET", None)
    env["TERM"] = "xterm-256color"
    return env


def run(binary, env, *args):
    result = subprocess.run([str(binary), *args], env=env, capture_output=True, timeout=30)
    if result.returncode:
        raise AssertionError(f"command failed: {args[0]} (exit {result.returncode})")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    print("Test executable SHA256:", hashlib.sha256(binary.read_bytes()).hexdigest())
    with tempfile.TemporaryDirectory(prefix="bmux-pairing-smoke-") as temp:
        root = Path(temp)
        host = environment(root / "host")
        client = environment(root / "client")
        # A direct-only fixture keeps this deterministic and independent of public infra.
        (Path(host["BMUX_CONFIG_DIR"]) / "bmux.toml").write_text(
            '[plugins.settings."bmux.connections".remote]\nbind_addr = "127.0.0.1:0"\n'
            '[plugins.settings."bmux.connections".remote.relay]\nmode = "disabled"\n'
        )
        terminals = []
        log = (root / "host.log").open("wb")
        server = subprocess.Popen(
            [str(binary), "server", "start", "--foreground-internal"],
            env=host, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
        )
        try:
            deadline = time.monotonic() + 30
            while True:
                status = subprocess.run([str(binary), "server", "status"], env=host, capture_output=True, timeout=5)
                if status.returncode == 0 and b"running" in status.stdout.lower():
                    break
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise AssertionError("isolated host did not start")
                time.sleep(0.1)
            run(binary, host, "remote", "enable", "--yes")
            run(binary, client, "remote", "list")
            assert not list(Path(client["BMUX_RUNTIME_DIR"]).glob("*.sock")), "client started server"
            owner = Terminal(binary, host, ["remote", "pair"])
            terminals.append(owner)
            link = owner.expect(rb"(bmux-pair:v1:[0-9a-f]+)", 30).group(1)
            connecting = Terminal(binary, client, ["connect", "--pair", "--name", "desktop", "--device-name", "mac"])
            terminals.append(connecting)
            connecting.expect(rb"Pairing link.*: ")
            connecting.send(link + b"\n")
            owner_phrase = owner.expect(rb"Compare this phrase on the other device: ([0-9a-f]{4}(?:-[0-9a-f]{4}){3})", 30).group(1)
            client_phrase = connecting.expect(rb"Verify on the host: ([0-9a-f]{4}(?:-[0-9a-f]{4}){3})", 30).group(1)
            assert owner_phrase == client_phrase, "verification phrase mismatch"
            owner.expect(rb"\[y/N\]")
            owner.send(b"y\n")
            connecting.expect(rb"Paired\. Saved host: desktop", 30)
            assert owner.process.wait(timeout=10) == 0
            time.sleep(1)
            connecting.expect(rb"\x1b\[\?2004h", 30)
            time.sleep(2)
            # The prompt is hidden, so command text must not appear as input echo.
            connecting.send(b"printf '\\102\\115\\125\\130\\137\\104\\101\\105\\115\\117\\116\\114\\105\\123\\123\\137\\120\\122\\117\\117\\106\\n'\r")
            connecting.expect(rb"BMUX_DAEMONLESS_PROOF", 45)
            run(binary, client, "remote", "status", "desktop")
            assert not list(Path(client["BMUX_RUNTIME_DIR"]).glob("*.sock")), "client started server"
            # Stop just the viewer: remote execution must survive.
            connecting.close()
            terminals.remove(connecting)
            resumed = Terminal(binary, client, ["connect", "desktop"])
            terminals.append(resumed)
            resumed.expect(rb"BMUX_DAEMONLESS_PROOF", 20)
            run(binary, host, "remote", "list")
            # Device naming/revocation is checked by the same public workflow.
            run(binary, host, "remote", "revoke", "mac")
            rejected = subprocess.run([str(binary), "attach", "paired-terminal://desktop/main"], env=client, capture_output=True, timeout=20)
            assert rejected.returncode != 0, "revoked client reauthenticated"
            print("PASS: interactive pair, daemonless attach/input/resume, revoke/reconnect denial")
        finally:
            for terminal in terminals:
                terminal.close()
            subprocess.run([str(binary), "server", "stop"], env=host, capture_output=True, timeout=15)
            if server.poll() is None:
                server.terminate()
            server.wait(timeout=10)
            log.close()


if __name__ == "__main__":
    main()
