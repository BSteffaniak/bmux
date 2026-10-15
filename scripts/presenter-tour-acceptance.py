#!/usr/bin/env python3
"""Rehearse the tour's actual embedded playbooks in isolated presenter processes.

Requires explicit prebuilt artifacts. Never attaches to an existing user runtime.
Private evidence retains PTY output and runner results, not public diagnostics.
"""
import argparse
from collections import deque
import importlib.util
import json
from pathlib import Path
import sys
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("presenter_helpers", ROOT / "scripts/presenter-web-acceptance.py")
H = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(H)


class Tour(H.Acceptance):
    def prepare_deck(self):
        # Each case below is a one-slide view of the real checked-in source.
        self.deck.write_text("# Tour rehearsal\n")

    def run_demo(self, name):
        source = ROOT / "presentations/bmux-tour/demos" / f"{name}.dsl"
        (self.root / source.name).write_bytes(source.read_bytes())
        self.deck.write_text(
            f"# {name}\n\n<!-- bmux:block-id demo-{name} -->\n"
            f'<!-- bmux:component playbook-demo {{"playbook":"{source.name}",'
            f'"title":"{name}","on_leave":"stop"}} -->\n')
        process = self.spawn(["present", "play", str(self.deck), "--allow-playbooks",
                              "--web", "--web-port", "0", "--web-assets", str(self.args.assets), "--no-reload"], mirror=True)
        url = self.url(process)
        origin, token = self.endpoint(url)
        browser = self.browser(url)
        before = set(self.root.rglob("runner.json"))
        process.send(b"\t")
        process.send(b"o")
        time.sleep(0.3)
        process.send(b"r")
        observed = deque(maxlen=64)

        def settled():
            status, body = H.WEB.http(origin, "/snapshot", token)
            if status == 200:
                text = H.WEB.text(json.loads(body))
                if not observed or observed[-1] != text:
                    observed.append(text)
            for path in set(self.root.rglob("runner.json")) - before:
                result = json.loads(path.read_text())
                if result.get("phase") in ("script-complete", "failed", "completed", "settled"):
                    return path, result
            return None

        path, result = H.wait_for(f"{name} managed completion", settled, 90)
        (self.root / f"{name}-observed.txt").write_text("\n---\n".join(observed))
        H.require(not result.get("diagnostic"), f"{name} managed run failed")
        frame = json.loads(H.WEB.http(origin, "/snapshot", token)[1])
        self.commit(browser, frame)
        H.require(result.get("phase") == "script-complete", f"{name} result did not pass")
        H.require(any(name in text for text in observed), f"{name} audience output missing")
        process.send(b"q")
        H.wait_for(f"{name} presenter exit", lambda: process.child.poll() is not None, 20)
        H.require(process.child.returncode == 0, f"{name} presenter exit failed")
        self.passed(f"tour {name}: real managed script and retained audience output")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "plugins", "assets", "browser"):
        parser.add_argument("--" + name, type=lambda value: Path(value).resolve(strict=True), required=True)
    parser.add_argument("--state-parent", type=Path, default=Path("/tmp"))
    parser.add_argument("--demos", nargs="+", choices=("panes", "navigation", "finder", "themes", "rendering", "evidence"),
                        default=["panes", "navigation", "finder", "themes", "rendering", "evidence"])
    args = parser.parse_args()
    suite = Tour(args)
    print(f"Private tour evidence: {suite.root}")
    try:
        for name in args.demos:
            suite.run_demo(name)
    except BaseException as error:
        suite.failed = type(error).__name__
        (suite.root / "failure.txt").write_text(str(error))
    finally:
        suite.cleanup()
        return 0 if suite.failed is None else 1


if __name__ == "__main__":
    sys.exit(main())
