#!/usr/bin/env python3
"""Isolated scratch-image startup checks. Argument: explicit pinned-Nix binary."""
import hashlib
import io
import json
import pathlib
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
import uuid


def docker(*args, **kwargs):
    return subprocess.run(["docker", *args], check=True, capture_output=True, **kwargs).stdout


def main():
    binary = pathlib.Path(sys.argv[1]).resolve()
    print("test executable sha256:", hashlib.sha256(binary.read_bytes()).hexdigest())
    tag = "bmux-relay-startup-test:" + uuid.uuid4().hex
    volume = "bmux-relay-startup-test-" + uuid.uuid4().hex
    containers = []
    with tempfile.TemporaryDirectory(prefix="bmux-relay-container-") as temp:
        context = pathlib.Path(temp)
        root = context / "artifacts/live-relay-rootfs"
        subprocess.run(["bash", "packages/live-relay/package-rootfs.sh", str(binary), str(root)], check=True)
        (context / "Dockerfile").write_bytes(pathlib.Path("packages/live-relay/Dockerfile").read_bytes())
        try:
            docker("build", "-t", tag, str(context))
            docker("volume", "create", volume)
            def create():
                name = "bmux-relay-startup-test-" + uuid.uuid4().hex
                containers.append(name)
                docker("create", "--name", name, "-p", "127.0.0.1::8080", "-v", volume + ":/data",
                       "-e", "PUBLISHER_KEY_SHA256=" + hashlib.sha256(b"test-publisher").hexdigest(),
                       "-e", "METRICS_KEY_SHA256=" + hashlib.sha256(b"test-metrics").hexdigest(), tag)
                return name
            name = create()
            for _ in range(2):
                docker("start", name)
                port = json.loads(docker("inspect", name))[0]["NetworkSettings"]["Ports"]["8080/tcp"][0]["HostPort"]
                for attempt in range(100):
                    try:
                        with urllib.request.urlopen("http://127.0.0.1:" + port + "/health", timeout=1) as response:
                            assert json.load(response)["renderer"] == "bmwp-4"
                        break
                    except (OSError, ValueError):
                        if attempt == 99:
                            raise AssertionError(docker("logs", name))
                        time.sleep(0.05)
                pid = json.loads(docker("inspect", name))[0]["State"]["Pid"]
                status = pathlib.Path(f"/proc/{pid}/status").read_text()
                for field in ("Uid:", "Gid:"):
                    ids = next(line.split()[1:] for line in status.splitlines() if line.startswith(field))
                    assert ids == ["65532"] * 4, status
                assert "NoNewPrivs:\t1" in status, status
                archive = docker("cp", name + ":/data/relay/.", "-")
                with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
                    authority = next(item for item in tar.getmembers() if item.name.endswith("authority.json"))
                    assert (authority.uid, authority.gid, authority.mode) == (65532, 65532, 0o600), authority
                docker("stop", "-t", "2", name)
            # Refuse a foreign/symlink authority entry on this owned test volume.
            payload = io.BytesIO()
            with tarfile.open(fileobj=payload, mode="w") as tar:
                entry = tarfile.TarInfo("foreign-symlink")
                entry.type = tarfile.SYMTYPE
                entry.linkname = "/tmp"
                tar.addfile(entry)
            docker("cp", "-", name + ":/data/relay", input=payload.getvalue())
            docker("start", name)
            assert docker("wait", name).strip() != b"0"
            result = subprocess.run(["docker", "logs", name], check=True, capture_output=True)
            logs = (result.stdout + result.stderr).decode()
            assert "foreign authority entry" in logs, logs
            print("PASS: fresh root-owned volume, UID/GID drop, restart, private metadata, symlink/foreign-entry refusal")
        finally:
            for name in containers:
                subprocess.run(["docker", "rm", "-f", name], check=False, capture_output=True)
            subprocess.run(["docker", "volume", "rm", volume], check=False, capture_output=True)
            subprocess.run(["docker", "image", "rm", tag], check=False, capture_output=True)


if __name__ == "__main__":
    main()
