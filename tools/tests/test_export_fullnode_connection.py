import argparse
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


loader = importlib.machinery.SourceFileLoader(
    "exporter", str(Path(__file__).resolve().parents[1] / "export-fullnode-connection")
)
spec = importlib.util.spec_from_loader(loader.name, loader)
exporter = importlib.util.module_from_spec(spec)
loader.exec_module(exporter)


class ExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "config/ssl/ca").mkdir(parents=True)
        for name in ("chia_ca.crt", "chia_ca.key", "private_ca.crt", "private_ca.key"):
            (self.root / "config/ssl/ca" / name).write_text(name)
        self.config = {
            "selected_network": "ephemeral-test",
            "full_node": {"port": 58445, "rpc_port": 8555},
            "network_overrides": {"constants": {"ephemeral-test": {
                "GENESIS_CHALLENGE": "ab" * 32, "HARD_FORK2_HEIGHT": 9216
            }}},
        }
        self.args = argparse.Namespace(chia_root=None, node_host=None, output=None)

    def run_export(self, generate=None):
        (self.root / "config/config.yaml").write_text(json.dumps(self.config))
        def fake_cert(ca, key, crt_path, key_path):
            crt_path.write_bytes(b"leaf signed by " + ca)
            key_path.write_bytes(b"dedicated leaf key")
        with patch.dict(os.environ, {"CHIA_ROOT": str(self.root)}), patch.object(
            exporter, "detect_host", return_value="192.168.1.25"
        ), patch.object(exporter.importlib.metadata, "version", return_value="test-version"):
            previous = Path.cwd()
            try:
                os.chdir(self.root)
                return exporter.export(self.args, json.loads, generate or fake_cert).resolve()
            finally:
                os.chdir(previous)

    def test_defaults_preserve_network_and_export_only_leaf_keys(self):
        output = self.run_export()
        self.assertEqual(output.name, "fullnode-ephemeral-test-connection")
        manifest = (output / "connection.toml").read_text()
        self.assertIn('host = "192.168.1.25"', manifest)
        self.assertIn('port = 58445', manifest)
        self.assertIn('network = "ephemeral-test"', manifest)
        self.assertIn('host = "127.0.0.1"\nport = 18555', manifest)
        metadata = json.loads((output / "network.json").read_text())
        self.assertEqual(metadata["constants"]["HARD_FORK2_HEIGHT"], 9216)
        self.assertEqual(metadata["chia_version"], "test-version")
        self.assertEqual(output.stat().st_mode & 0o777, 0o700)
        for path in output.iterdir():
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertNotIn(b"chia_ca.key", path.read_bytes())
            self.assertNotIn(b"private_ca.key", path.read_bytes())

    def test_overrides_and_no_overwrite(self):
        self.args.chia_root = str(self.root)
        self.args.node_host = "node.example.test"
        self.args.output = str(self.root / "chosen")
        output = self.run_export()
        self.assertIn('host = "node.example.test"', (output / "connection.toml").read_text())
        original = (output / "client.key").read_bytes()
        with self.assertRaises(FileExistsError):
            self.run_export()
        self.assertEqual((output / "client.key").read_bytes(), original)

    def test_failed_generation_removes_partial_bundle(self):
        def fail(*args):
            raise ValueError("fixture signing failure")
        with self.assertRaisesRegex(ValueError, "fixture signing failure"):
            self.run_export(fail)
        self.assertFalse((self.root / "fullnode-ephemeral-test-connection").exists())

    def test_network_mismatch_and_invalid_port_rejected(self):
        self.config["full_node"]["selected_network"] = "other"
        with self.assertRaisesRegex(ValueError, "disagree"):
            self.run_export()
        del self.config["full_node"]["selected_network"]
        self.config["full_node"]["port"] = 0
        with self.assertRaisesRegex(ValueError, "port"):
            self.run_export()

    def test_missing_root_has_actionable_error(self):
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(ValueError, "--chia-root or set CHIA_ROOT"):
                exporter.export(self.args, json.loads, None)

    def test_fullnode_specific_overrides_take_precedence(self):
        self.config["full_node"]["network_overrides"] = {
            "constants": {"ephemeral-test": {"HARD_FORK2_HEIGHT": 123}}
        }
        output = self.run_export()
        metadata = json.loads((output / "network.json").read_text())
        self.assertEqual(metadata["constants"]["HARD_FORK2_HEIGHT"], 123)


if __name__ == "__main__":
    unittest.main()
