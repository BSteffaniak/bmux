#!/usr/bin/env python3
"""Focused lexical guard for shared execution/frontend platform boundaries.

Not a Rust parser or a proof of behavior. Backend modules remain responsible for
OS safety. Missing guarded files fail closed; comments, strings and cfg(test)
items are excluded without relying on the host's active cfg.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys

SHARED_FILES = (
    "plugins/pane-runtime-plugin/src/runtime/supervisor.rs",
    "plugins/pane-runtime-plugin/src/runtime/environment.rs",
    "plugins/recording-plugin/src/interactive_command.rs",
    "plugins/recording-plugin/src/interactive_terminal.rs",
)

# Keep newlines/offsets so diagnostics point at the original source.
def blank(text: str) -> str:
    return re.sub(r"[^\n]", " ", text)


def code_only(source: str) -> str:
    """Mask nested block comments, line comments and Rust string/char literals."""
    output = list(source)
    index = 0
    while index < len(source):
        start = index
        if source.startswith("//", index):
            end = source.find("\n", index)
            index = len(source) if end < 0 else end
        elif source.startswith("/*", index):
            depth = 1
            index += 2
            while index < len(source) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
        elif match := re.match(r'(?:br|cr|r)(#*)"', source[index:]):
            terminator = '"' + match[1]
            end = source.find(terminator, index + match.end())
            index = len(source) if end < 0 else end + len(terminator)
        elif source[index] == '"':
            index += 1
            while index < len(source):
                if source[index] == "\\":
                    index += 2
                elif source[index] == '"':
                    index += 1
                    break
                else:
                    index += 1
        elif match := re.match(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'", source[index:]):
            index += match.end()
        else:
            index += 1
            continue
        output[start:index] = blank(source[start:index])
    return "".join(output)


def closing(code: str, start: int, left: str, right: str) -> int:
    depth = 0
    for index in range(start, len(code)):
        if code[index] == left:
            depth += 1
        elif code[index] == right:
            depth -= 1
            if depth == 0:
                return index + 1
    raise ValueError(f"unclosed {left} at offset {start}")


def without_tests(code: str) -> str:
    # Only explicit positive test gates are exempt. cfg(not(test)) and arbitrary
    # cfg expressions cannot hide production code from this check.
    pattern = re.compile(r"#\s*\[\s*cfg\s*\(\s*(?:test\s*|all\s*\(\s*test\s*(?:,\s*(?:unix|windows)\s*)?\))\)\s*\]")
    output = list(code)
    for match in pattern.finditer(code):
        if not "".join(output[match.start():match.end()]).strip():
            continue
        index = match.end()
        while True:
            while index < len(code) and code[index].isspace():
                index += 1
            if code.startswith("#[", index):
                index = closing(code, index + 1, "[", "]")
            else:
                break
        # Test modules/functions/blocks have a body; test imports end at ';'.
        while index < len(code) and code[index] not in "{;,":
            index += 1
        if index < len(code) and code[index] == "{":
            end = closing(code, index, "{", "}")
            # A grouped use has a trailing semicolon, harmless if left behind.
        else:
            end = min(index + 1, len(code))
        output[match.start():end] = blank(code[match.start():end])
    return "".join(output)


FORBIDDEN = (
    (r"\b(?:std|core|tokio)\s*::\s*os\s*::", "raw OS namespace"),
    (r"\bos\s*::\s*(?:unix|windows|fd)\b", "grouped raw OS namespace"),
    (r"\bsignal\s*::\s*(?:unix|windows|ctrl_c)\b|\bSIG(?:TERM|KILL|INT|HUP|QUIT|PIPE)\b", "grouped native signal API"),
    (r"\b(?:libc|windows_sys|windows|winapi|nix|rustix)\s*::", "raw OS crate"),
    (r"\btokio\s*::\s*signal\b", "native signal registration"),
    (r"\b(?:AsRawFd|FromRawFd|OwnedFd|RawFd|AsRawHandle|FromRawHandle|OwnedHandle|RawHandle|OpenOptionsExt|PermissionsExt|CommandExt)\b", "raw OS extension/handle"),
    (r"\b(?:killpg|pthread_kill|sigaction|sigprocmask|GenerateConsoleCtrlEvent|TerminateProcess|TerminateJobObject|SetConsoleCtrlHandler)\b", "native signal/process operation"),
    (r"(?<![\w.])(?:kill|signal|raise)\s*\(", "raw signal call"),
    (r"\bextern\s+(?:\{)?", "native ABI declaration"),
)


def violations(source: str) -> list[tuple[int, str]]:
    code = without_tests(code_only(source))
    found = []
    for pattern, reason in FORBIDDEN:
        for match in re.finditer(pattern, code):
            found.append((code.count("\n", 0, match.start()) + 1, reason))
    # Platform cfg is permitted only on a whole module selector. No cfg blocks,
    # functions, imports, platform-specific fields or cfg! branches in shared code.
    for match in re.finditer(r"#\s*\[\s*cfg(?:_attr)?\s*\(|\bcfg!\s*\(", code):
        start = match.start()
        end = closing(code, code.index("(", start), "(", ")")
        attribute = code[start:end]
        if not re.search(r"\b(?:unix|windows|target_os|target_family|target_env|target_arch)\b", attribute):
            continue
        rest = code[end:]
        rest = re.sub(r"^\s*\]", "", rest, count=1)
        # Skip inert attributes such as #[path = "backend/unix.rs"].
        rest = re.sub(r"^(?:\s*#\[[^\]]*\])*", "", rest)
        selector = re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*;", rest)
        if not selector or "cfg!" in attribute or "cfg_attr" in attribute:
            found.append((code.count("\n", 0, start) + 1, "platform cfg outside whole-module selector"))
    return sorted(set(found))


def check(root: Path) -> list[str]:
    errors = []
    for relative in SHARED_FILES:
        path = root / relative
        try:
            source = path.read_text(encoding="utf-8")
            errors.extend(f"{relative}:{line}: {reason}" for line, reason in violations(source))
        except (OSError, ValueError) as error:
            errors.append(f"{relative}: cannot check: {error}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    errors = check(args.root)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"platform boundaries: PASS ({len(SHARED_FILES)} shared files; lexical guard, not runtime proof)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
