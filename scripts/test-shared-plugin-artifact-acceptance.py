#!/usr/bin/env python3
"""Failure-path tests for the artifact inspector; no compilation/subprocesses.

Callback doubles exercise harness rejection paths, not real artifact parity.
The inspector itself must subsequently run against X1's actual native libraries.
"""
import ctypes
import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "artifact_acceptance", pathlib.Path(__file__).with_name("shared-plugin-artifact-acceptance.py"))
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


class ArtifactAcceptanceTests(unittest.TestCase):
    def descriptor(self, plugin_id="bcode.file-references", manifest_id=None):
        value = {"plugin_id": plugin_id, "version": "1", "roles": ["frontend"]}
        if manifest_id:
            value["manifest_toml"] = f'id = "{manifest_id}"\n'
        encoded = json.dumps(value).encode()

        @HARNESS.MetadataFn
        def metadata(output, capacity, length):
            length[0] = len(encoded)
            if capacity < len(encoded):
                return HARNESS.BUFFER_TOO_SMALL
            ctypes.memmove(output, encoded, len(encoded))
            return HARNESS.OK

        descriptor = HARNESS.Descriptor()
        descriptor.size = ctypes.sizeof(HARNESS.Descriptor)
        descriptor.version = 2
        descriptor.metadata = metadata
        # Keep native callbacks and structures alive through the mocked load.
        self.addCleanup(lambda: (descriptor, metadata))
        return descriptor

    def inspect(self, descriptor):
        entry = mock.Mock(return_value=ctypes.addressof(descriptor))
        library = mock.Mock(bmux_plugin_entry_v2=entry)
        with tempfile.TemporaryDirectory() as directory:
            artifact = pathlib.Path(directory) / "selected-artifact"
            artifact.write_bytes(b"test-double-not-a-native-library")
            with mock.patch.object(HARNESS.ctypes, "CDLL", return_value=library) as load:
                result = HARNESS.inspect_artifact("bcode.file-references", artifact, 2)
                load.assert_called_once_with(str(artifact.resolve()))
                return result

    def test_wrong_embedded_plugin_identity_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "embedded identity mismatch"):
            self.inspect(self.descriptor(plugin_id="bcode.filesystem"))

    def test_correct_top_level_id_does_not_hide_wrong_manifest(self):
        with self.assertRaisesRegex(ValueError, "embedded manifest identity mismatch"):
            self.inspect(self.descriptor(manifest_id="bcode.filesystem"))

    def test_correct_identity_records_artifact_hash_and_sample_count(self):
        result = self.inspect(self.descriptor(manifest_id="bcode.file-references"))
        self.assertEqual(result["expected_id"], "bcode.file-references")
        self.assertEqual(len(result["sha256"]), 64)
        self.assertEqual(result["samples"], 2)

    def test_wrong_version_rejected_before_metadata_call(self):
        descriptor = self.descriptor()
        descriptor.version = 1
        with self.assertRaisesRegex(ValueError, "unsupported descriptor"):
            self.inspect(descriptor)

    def test_short_descriptor_rejected_before_reading_later_fields(self):
        descriptor = self.descriptor()
        descriptor.size = ctypes.sizeof(HARNESS.DescriptorHeader)
        with self.assertRaisesRegex(ValueError, "unsupported descriptor"):
            self.inspect(descriptor)

    def test_all_current_scoped_capabilities_are_accepted(self):
        descriptor = self.descriptor()
        descriptor.capabilities = 15
        result = self.inspect(descriptor)
        self.assertEqual(result["capabilities"], 15)

    def test_registration_capability_is_known(self):
        descriptor = self.descriptor()
        descriptor.capabilities = 8
        self.assertEqual(self.inspect(descriptor)["capabilities"], 8)

    def test_unknown_capabilities_are_rejected_before_metadata(self):
        descriptor = self.descriptor()
        descriptor.capabilities = 16
        with self.assertRaisesRegex(ValueError, "unsupported native capability bits"):
            self.inspect(descriptor)

    def test_descriptor_uses_scoped_start_slot_before_metadata(self):
        # Independent C-compatible layout, not another use of Descriptor's own
        # fields: catches accidentally interpreting start_scoped as metadata.
        @HARNESS.MetadataFn
        def metadata(output, capacity, length):
            payload = b'{"plugin_id":"bcode.file-references","version":"1","roles":["frontend"]}'
            length[0] = len(payload)
            if capacity < len(payload):
                return HARNESS.BUFFER_TOO_SMALL
            ctypes.memmove(output, payload, len(payload))
            return HARNESS.OK
        pointer = ctypes.sizeof(ctypes.c_void_p)
        raw = ctypes.create_string_buffer(16 + 9 * pointer)
        ctypes.c_uint32.from_buffer(raw, 0).value = len(raw)
        ctypes.c_uint32.from_buffer(raw, 4).value = 2
        ctypes.c_uint64.from_buffer(raw, 8).value = 15
        ctypes.c_void_p.from_buffer(raw, 16).value = 1  # Never callable: scoped start.
        ctypes.c_void_p.from_buffer(raw, 16 + pointer).value = ctypes.cast(metadata, ctypes.c_void_p).value
        descriptor = ctypes.cast(raw, ctypes.POINTER(HARNESS.Descriptor)).contents
        self.assertEqual(self.inspect(descriptor)["capabilities"], 15)

    def test_ports_v2_contains_registration_after_host_ack(self):
        pointer = ctypes.sizeof(ctypes.c_void_p)
        self.assertEqual(HARNESS.PORTS_VERSION, 2)
        self.assertEqual(HARNESS.InvocationPorts.registration_begin.offset, 24 + 6 * pointer)
        self.assertEqual(ctypes.sizeof(HARNESS.InvocationPorts), 24 + 7 * pointer)

    def test_oversized_metadata_rejected_before_allocating_buffer(self):
        @HARNESS.MetadataFn
        def metadata(_output, _capacity, length):
            length[0] = HARNESS.MAX_BYTES + 1
            return HARNESS.BUFFER_TOO_SMALL
        descriptor = self.descriptor()
        descriptor.metadata = metadata
        with self.assertRaisesRegex(ValueError, "invalid metadata size response"):
            self.inspect(descriptor)

    def test_retrieval_length_cannot_exceed_host_buffer(self):
        @HARNESS.MetadataFn
        def metadata(_output, capacity, length):
            length[0] = 8 if capacity == 0 else capacity + 1
            return HARNESS.BUFFER_TOO_SMALL if capacity == 0 else HARNESS.OK
        descriptor = self.descriptor()
        descriptor.metadata = metadata
        with self.assertRaisesRegex(ValueError, "invalid metadata retrieval"):
            self.inspect(descriptor)


if __name__ == "__main__":
    unittest.main()
