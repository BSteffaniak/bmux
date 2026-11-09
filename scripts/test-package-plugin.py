#!/usr/bin/env python3
"""Focused packaging tests: no Cargo, native build, daemon or user installation."""
import copy
import ctypes
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


pack = load("packaging_under_test", "package-plugin.py")
acceptance = load("acceptance_under_test", "package-plugin-acceptance.py")
artifact_acceptance = load("artifact_acceptance_under_test", "shared-plugin-artifact-acceptance.py")


class DescriptorTests(unittest.TestCase):
    def descriptor(self, version=2, capabilities=15, size=None):
        callback_type = ctypes.CFUNCTYPE(ctypes.c_int32, ctypes.c_void_p, ctypes.c_size_t,
                                         ctypes.POINTER(ctypes.c_size_t))
        payload = json.dumps({"plugin_id": "acceptance.scoped-loader", "version": "1.0.0",
                              "roles": ["backend"]}).encode()

        def metadata(output, capacity, required):
            required[0] = len(payload)
            if capacity < len(payload):
                return 3
            ctypes.memmove(output, payload, len(payload))
            return 0

        class Descriptor(ctypes.Structure):
            _fields_ = [("size", ctypes.c_uint32), ("version", ctypes.c_uint32),
                        ("capabilities", ctypes.c_uint64)] + [
                (name, ctypes.c_void_p) for name in
                ("start_scoped", "metadata", "create", "destroy", "start", "read", "cancel", "ack")]
            if version == 3:
                _fields_ += [("declarations", ctypes.c_void_p)]
            if version == 3 and capabilities & 2048:
                _fields_ += [("read_wake", ctypes.c_void_p)]

        callback = callback_type(metadata)
        table = Descriptor(ctypes.sizeof(Descriptor) if size is None else size,
                           version, capabilities, 1, ctypes.cast(callback, ctypes.c_void_p),
                           1, 1, 1, 1, 1, 1, *([1] if version == 3 else []),
                           *([1] if version == 3 and capabilities & 2048 else []))

        class Entry:
            def __call__(self):
                return ctypes.addressof(table)

        class Library:
            bmux_plugin_entry_v2 = Entry()

        return Library(), table, callback

    def test_scoped_descriptor_layout_and_all_capabilities(self):
        library, table, callback = self.descriptor()
        with patch.object(pack.ctypes, "CDLL", return_value=library):
            observed = pack.artifact_metadata(Path(__file__))
        self.assertEqual(observed["capabilities"], 15)
        self.assertEqual(observed["metadata"]["plugin_id"], "acceptance.scoped-loader")

    def test_v3_descriptor_and_declarations_required(self):
        library, table, callback = self.descriptor(version=3, capabilities=31)
        with patch.object(pack.ctypes, "CDLL", return_value=library):
            self.assertEqual(pack.artifact_metadata(Path(__file__))["version"], 3)
            table.declarations = None
            with self.assertRaisesRegex(RuntimeError, "declarations"):
                pack.artifact_metadata(Path(__file__))

    def test_identity_harness_negotiates_v2_v3_and_rejects_wrong_id(self):
        for version, capabilities in ((2, 15), (3, 31)):
            library, table, callback = self.descriptor(version=version, capabilities=capabilities)
            with patch.object(artifact_acceptance.ctypes, "CDLL", return_value=library):
                observed = artifact_acceptance.inspect_artifact("acceptance.scoped-loader", Path(__file__), 2)
                self.assertEqual(observed["abi_version"], version)
                with self.assertRaisesRegex(ValueError, "identity mismatch"):
                    artifact_acceptance.inspect_artifact("bcode.filesystem", Path(__file__), 2)

    def test_supported_v3_profiles_preserve_observed_capabilities(self):
        for capabilities in (31, 127, 255, 511, 1023, 2047, 4095):
            with self.subTest(capabilities=capabilities):
                library, table, callback = self.descriptor(version=3, capabilities=capabilities)
                with patch.object(pack.ctypes, "CDLL", return_value=library):
                    observed = pack.artifact_metadata(Path(__file__))
                self.assertEqual(observed["capabilities"], capabilities)
                self.assertEqual(observed["version"], 3)

    def test_host_wake_profile_requires_read_wake_tail(self):
        library, table, callback = self.descriptor(version=3, capabilities=4095)
        with patch.object(pack.ctypes, "CDLL", return_value=library):
            self.assertEqual(pack.artifact_metadata(Path(__file__))["capabilities"], 4095)
            table.read_wake = None
            with self.assertRaisesRegex(RuntimeError, "read_wake"):
                pack.artifact_metadata(Path(__file__))

    def test_missing_scoped_operation_rejected(self):
        library, table, callback = self.descriptor()
        table.start_scoped = None
        with patch.object(pack.ctypes, "CDLL", return_value=library):
            with self.assertRaisesRegex(RuntimeError, "incomplete"):
                pack.artifact_metadata(Path(__file__))

    def test_descriptor_version_size_and_missing_or_unknown_capabilities_rejected(self):
        for arguments in ({"version": 1}, {"version": 4}, {"size": 8}, {"capabilities": 7}, {"capabilities": 31},
                          {"version": 3, "capabilities": 15}, {"version": 3, "capabilities": 63},
                          {"version": 3, "capabilities": 1024}, {"version": 3, "capabilities": 3071},
                          {"version": 3, "capabilities": 2048}, {"version": 3, "capabilities": 6143},
                          {"version": 3, "capabilities": 8191}, {"version": 2, "capabilities": 2063},
                          {"version": 3, "capabilities": 4095, "size": 88},
                          {"version": 3, "capabilities": 31, "size": 80}):
            with self.subTest(arguments=arguments):
                library, table, callback = self.descriptor(**arguments)
                with patch.object(pack.ctypes, "CDLL", return_value=library):
                    with self.assertRaises(RuntimeError):
                        pack.artifact_metadata(Path(__file__))


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.library = self.root / "fixture.dylib"
        self.library.write_bytes(b"not executable: metadata is mocked")
        self.declaration = self.root / "plugin.toml"
        self.declaration.write_text('id = "acceptance.example"\nversion = "1.0.0"\nrequired_capabilities = ["example.read"]\n')
        self.observed = {"version": 2, "capabilities": 15, "metadata": {
            "plugin_id": "acceptance.example", "version": "1.0.0", "roles": ["backend"],
            "required_capabilities": ["example.read"], "contracts": [],
            "manifest_toml": self.declaration.read_text()}}

    def publish(self):
        with patch.object(pack, "artifact_metadata", return_value=self.observed):
            return pack.install(self.library, self.declaration, self.root / "install", "backend", "build-label")

    def test_current_v3_receipt_preserves_profile_and_declaration(self):
        self.observed["version"] = 3
        self.observed["capabilities"] = 2047
        destination = self.publish()
        found = pack.discover(self.root / "install")
        self.assertEqual(found[0]["shared_descriptor"], self.observed)
        self.assertEqual((destination / self.declaration.name).read_bytes(),
                         self.declaration.read_bytes())

    @unittest.skipIf(sys.platform == "win32", "POSIX durable publisher")
    def test_immutable_receipt_repeat_inventory_and_tamper_rejection(self):
        destination = self.publish()
        receipt = (destination / "artifact.json").read_bytes()
        self.assertEqual(self.publish(), destination)
        self.assertEqual((destination / "artifact.json").read_bytes(), receipt)
        found = pack.discover(self.root / "install")
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0]["shared_descriptor"], self.observed)
        (destination / self.library.name).write_bytes(b"tampered")
        with self.assertRaises(RuntimeError):
            pack.discover(self.root / "install")
        with self.assertRaises(RuntimeError):
            self.publish()

    @unittest.skipIf(sys.platform == "win32", "POSIX durable publisher")
    def test_receipt_mismatch_is_rejected_without_overwriting_it(self):
        destination = self.publish()
        receipt_path = destination / "artifact.json"
        value = json.loads(receipt_path.read_text())
        value["shared_descriptor"]["capabilities"] = 7
        receipt_path.write_text(json.dumps(value))
        corrupted = receipt_path.read_bytes()
        with self.assertRaises(RuntimeError):
            pack.discover(self.root / "install")
        with self.assertRaises(RuntimeError):
            self.publish()
        self.assertEqual(receipt_path.read_bytes(), corrupted)

    @unittest.skipIf(sys.platform == "win32", "POSIX durable publisher")
    def test_v3_receipt_preserves_negotiated_capabilities(self):
        self.observed["version"] = 3
        self.observed["capabilities"] = 31
        self.publish()
        inventory = pack.discover(self.root / "install")
        self.assertEqual(len(inventory), 1)
        self.assertEqual(inventory[0]["shared_descriptor"]["version"], 3)
        self.assertEqual(inventory[0]["shared_descriptor"]["capabilities"], 31)

    @unittest.skipIf(sys.platform == "win32", "POSIX durable publisher")
    def test_identity_role_version_declaration_and_capability_mismatches_do_not_publish(self):
        for field, value in (("plugin_id", "wrong"), ("version", "2.0.0"),
                             ("roles", ["frontend"]), ("manifest_toml", "wrong"),
                             ("required_capabilities", [])):
            with self.subTest(field=field):
                observed = copy.deepcopy(self.observed)
                observed["metadata"][field] = value
                with patch.object(pack, "artifact_metadata", return_value=observed):
                    with self.assertRaises(RuntimeError):
                        pack.install(self.library, self.declaration, self.root / "install", "backend", "label")
                self.assertFalse((self.root / "install").exists())


class GenerationTests(unittest.TestCase):
    def test_component_features_come_from_implementation_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            implementation = root / "impl/Cargo.toml"
            implementation.parent.mkdir()
            implementation.write_text(
                '[package]\nname = "demo_plugin"\nversion = "0.0.0"\n\n'
                '[package.metadata.bmux-plugin.components.backend]\nfeatures = ["native"]\n'
                '[package.metadata.bmux-plugin.components.frontend]\nfeatures = ["native", "presentation"]\n')
            sdk = ROOT / "packages/plugin-sdk/Cargo.toml"
            backend = tomllib.loads(pack.generate(
                implementation, sdk, root / "backend", "backend", [], "b").read_text())
            self.assertEqual(backend["dependencies"]["implementation"]["features"], ["native"])
            self.assertFalse(backend["dependencies"]["implementation"]["default-features"])
            frontend = tomllib.loads(pack.generate(
                implementation, sdk, root / "frontend", "frontend", ["native", "tui"], "f").read_text())
            # Declared features come first; explicit ones extend without duplicates.
            self.assertEqual(frontend["dependencies"]["implementation"]["features"],
                             ["native", "presentation", "tui"])
            self.assertEqual(json.loads((root / "frontend/packaging.json").read_text())["features"],
                             ["native", "presentation", "tui"])
            implementation.write_text(
                '[package]\nname = "demo_plugin"\nversion = "0.0.0"\n\n'
                '[package.metadata.bmux-plugin.components.backend]\nfeatures = "native"\n')
            with self.assertRaises(ValueError):
                pack.generate(implementation, sdk, root / "invalid", "backend", [], "x")

    def test_exact_fixture_entry_and_sibling_with_declared_dependencies(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "fixture"
            manifest = acceptance.fixture_package(ROOT, output, vars(pack))
            value = tomllib.loads(manifest.read_text())
            self.assertEqual(value["lib"]["crate-type"], ["cdylib"])
            self.assertEqual(set(value["dependencies"]), {"bmux_plugin_sdk", "scoped_loader_definition"})
            implementation = tomllib.loads((output / "implementation/Cargo.toml").read_text())
            self.assertEqual(implementation["lib"]["crate-type"], ["rlib"])
            self.assertEqual(value["workspace"].get("members"), ["implementation"])
            self.assertNotIn("workspace", implementation, "member must not declare a nested workspace root")
            self.assertNotIn("patch", implementation, "Cargo patches belong to the workspace root")
            dependency = value["dependencies"]["scoped_loader_definition"]
            self.assertEqual((output / dependency["path"]).resolve(), (output / "implementation").resolve())
            self.assertEqual(dependency["package"], implementation["package"]["name"])
            # Regenerating a failed earlier output must repair its workspace
            # composition too, not require deleting evidence or choosing a new root.
            (output / "implementation/Cargo.toml").write_text(
                (output / "implementation/Cargo.toml").read_text() + "\n[workspace]\n")
            acceptance.fixture_package(ROOT, output, vars(pack))
            repaired = tomllib.loads((output / "implementation/Cargo.toml").read_text())
            self.assertNotIn("workspace", repaired)
            self.assertEqual(set(implementation["dependencies"]), {"bmux_plugin_sdk", "serde_json"})
            self.assertEqual((output / "implementation/lib.rs").read_bytes(),
                             (ROOT / "packages/plugin/tests/fixtures/scoped_loader_definition.rs").read_bytes())
            wrapper = (output / "src/scoped_loader_native.rs").read_text()
            self.assertIn("export_plugin_entrypoint!(scoped_loader_definition::plugin_definition)", wrapper)
            self.assertNotIn("mod scoped_loader_definition", wrapper)
            identity = json.loads((output / "packaging.json").read_text())
            self.assertEqual(identity["abi_version"], pack.ABI_VERSION)
            self.assertEqual(identity["required_capabilities"], pack.REQUIRED_CAPABILITIES)
            self.assertEqual(identity["expected_id"], "acceptance.scoped-loader")

    def test_driver_generates_fixture_and_real_bcode_wrapper_without_cargo(self):
        bcode = ROOT.parents[1] / "wt-bcode/shared-plugin-architecture"
        self.assertTrue(bcode.is_dir(), "required approved sibling Bcode worktree unavailable")
        with tempfile.TemporaryDirectory() as temporary:
            with patch.dict("os.environ", {"PATH": ""}):
                subprocess.run([sys.executable, str(ROOT / "scripts/package-plugin-acceptance.py"),
                                "--bcode-root", str(bcode), "--output", temporary], check=True)
            source = (Path(temporary) / "file-references/src/lib.rs").read_text()
            self.assertIn("implementation::plugin_definition()", source)
            manifest = tomllib.loads((Path(temporary) / "file-references/Cargo.toml").read_text())
            self.assertEqual(manifest["dependencies"]["implementation"]["package"], "bcode_file_references_plugin")
            manifest = tomllib.loads((Path(temporary) / "hello/Cargo.toml").read_text())
            self.assertEqual(manifest["dependencies"]["implementation"]["package"], "bmux_hello_plugin")
            for domain in ("filesystem", "shell"):
                generated = Path(temporary) / domain
                manifest = tomllib.loads((generated / "Cargo.toml").read_text())
                self.assertEqual(manifest["package"]["name"], f"bcode_{domain}_plugin_dynamic_backend")
                self.assertEqual(manifest["dependencies"]["implementation"]["package"], f"bcode_{domain}_plugin")
                self.assertEqual(manifest["lib"]["crate-type"], ["cdylib"])
                self.assertIn("implementation::plugin_definition()", (generated / "src/lib.rs").read_text())
            self.assertFalse((Path(temporary) / "acceptance-artifacts.json").exists())

    def test_built_artifact_report_records_exact_filesystem_and_shell_paths(self):
        bcode = ROOT.parents[1] / "wt-bcode/shared-plugin-architecture"
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            def built(manifest, release, target):
                # Build-result plumbing only: no native code is loaded here.
                return manifest.parent / "cargo-selected-native-artifact"
            with patch.object(acceptance.runpy, "run_path", return_value={**vars(pack), "build": built}):
                with patch.object(sys, "argv", ["acceptance", "--bcode-root", str(bcode),
                                               "--output", str(output), "--build"]):
                    acceptance.main()
            report = json.loads((output / "acceptance-artifacts.json").read_text())
            self.assertEqual(len(report["artifacts"]), 5)
            for domain in ("filesystem", "shell"):
                artifact = str(output / domain / "cargo-selected-native-artifact")
                self.assertEqual(report["environment"][f"BCODE_{domain.upper()}_WRAPPER_ARTIFACT"], artifact)
                self.assertIn({"expected_id": f"bcode.{domain}", "path": artifact}, report["artifacts"])

    def test_bcode_entrypoint_delegates_without_cargo(self):
        bcode = ROOT.parents[1] / "wt-bcode/shared-plugin-architecture"
        self.assertTrue(bcode.is_dir(), "required approved sibling Bcode worktree unavailable")
        with tempfile.TemporaryDirectory() as temporary:
            with patch.dict("os.environ", {"PATH": "", "BMUX_PLUGIN_PACKAGER": str(ROOT / "scripts/package-plugin.py")}):
                subprocess.run([sys.executable, str(bcode / "scripts/package-plugin-acceptance.py"),
                                "--output", temporary], check=True)
            wrapper = (Path(temporary) / "file-references/src/lib.rs").read_text()
            self.assertIn("implementation::plugin_definition()", wrapper)
            self.assertFalse((Path(temporary) / "acceptance-artifacts.json").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
