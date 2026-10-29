#!/usr/bin/env python3
"""Regression cases for the focused Rust platform-boundary guard."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "platform_boundaries", Path(__file__).with_name("check-platform-boundaries.py")
)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


class PlatformBoundaries(unittest.TestCase):
    def test_backend_calls_and_whole_module_selection_are_allowed(self):
        source = '''
#[cfg(unix)]
#[path = "backend/unix.rs"]
mod backend;
#[cfg(windows)]
#[path = "backend/windows.rs"]
mod backend;
fn shared() { backend::interrupt(); }
'''
        self.assertEqual(guard.violations(source), [])

    def test_raw_os_imports_signals_and_handles_are_rejected(self):
        for source in (
            "use std::os::unix::fs::OpenOptionsExt;",
            "use std :: os :: windows :: io::RawHandle;",
            "use windows_sys::Win32::Foundation::HANDLE;",
            "use libc::{SIGTERM, kill};",
            "use nix::sys::signal;",
            "fn stop() { tokio::signal::ctrl_c(); }",
            "fn stop() { kill(12, 9); }",
            "fn stop() { GenerateConsoleCtrlEvent(0, 0); }",
            'unsafe extern "system" { fn native(); }',
            "use std::{os::windows::io::OwnedHandle};",
            "use std::{os::unix::fs};",
            "use tokio::{signal::windows::ctrl_c};",
            "fn stop() { backend::send(SIGTERM); }",
        ):
            with self.subTest(source=source):
                self.assertTrue(guard.violations(source))

    def test_cfg_inline_implementations_and_cfg_macros_are_rejected(self):
        for source in (
            "#[cfg(unix)] fn shared() {}",
            "#[cfg(windows)] use backend::interrupt;",
            "fn shared() { #[cfg(unix)] { backend::unix(); } }",
            "fn shared() { if cfg!(windows) { backend::windows(); } }",
            "#[cfg_attr(windows, allow(dead_code))] fn shared() {}",
            "#[cfg(target_os = \"windows\")] struct Handle;",
            "#[cfg(not(test))] fn live() { libc::kill(1, 2); }",
        ):
            with self.subTest(source=source):
                self.assertTrue(guard.violations(source))

    def test_test_items_are_exempt_but_production_after_them_is_not(self):
        source = '''
#[cfg(test)]
mod tests { fn fixture() { libc::kill(1, 2); } }
#[cfg(test)]
fn fixture() { #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; } }
#[cfg(all(test, unix))]
use backend::{StartupChild, negotiate_guardian};
fn live() { libc::kill(1, 2); }
'''
        violations = guard.violations(source)
        self.assertTrue(violations)
        self.assertTrue(all(line == 8 for line, _reason in violations), violations)

    def test_comments_strings_raw_strings_chars_and_lifetimes(self):
        source = '''
// libc::kill(1, 2);
/* outer /* use windows_sys::X; */ tokio::signal::ctrl_c(); */
const TEXT: &str = r###"#[cfg(unix)] libc::kill(1, 2);"###;
const OTHER: &str = "std::os::unix";
fn shared<'a>(value: &'a str) { let quote = '\\''; backend::use_value(value); }
'''
        self.assertEqual(guard.violations(source), [])
        self.assertTrue(guard.violations(source + "\nlibc::kill(1, 2);"))

    def test_comment_masking_does_not_join_tokens_or_shift_lines(self):
        self.assertTrue(guard.violations("\nuse std /* boundary */ :: os :: unix::fs;"))
        self.assertEqual(guard.violations("\nlibc::kill(1, 2);")[0][0], 2)

    def test_missing_file_fails_closed_and_fixture_tree_is_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(len(guard.check(root)), len(guard.SHARED_FILES))
            for name in guard.SHARED_FILES:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("fn shared() { backend::call(); }", encoding="utf-8")
            self.assertEqual(guard.check(root), [])
            (root / guard.SHARED_FILES[0]).write_text("use libc::kill;", encoding="utf-8")
            self.assertEqual(len(guard.check(root)), 1)

    def test_real_checkout(self):
        self.assertEqual(guard.check(Path(__file__).resolve().parents[1]), [])


if __name__ == "__main__":
    unittest.main()
