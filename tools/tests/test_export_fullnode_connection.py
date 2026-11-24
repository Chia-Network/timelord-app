import argparse
import contextlib
import importlib.machinery
import importlib.metadata
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest.mock import patch


loader = importlib.machinery.SourceFileLoader(
    "exporter", str(Path(__file__).resolve().parents[1] / "export-fullnode-connection")
)
spec = importlib.util.spec_from_loader(loader.name, loader)
exporter = importlib.util.module_from_spec(spec)
loader.exec_module(exporter)


def fake_cert(ca, key, crt_path, key_path):
    crt_path.write_bytes(b"leaf signed by " + ca)
    key_path.write_bytes(b"dedicated leaf key")


def stub_modules(generate_cert, version=None):
    class YAMLError(Exception):
        pass

    yaml_module = types.ModuleType("yaml")
    yaml_module.safe_load = json.loads
    yaml_module.YAMLError = YAMLError
    chia = types.ModuleType("chia")
    if version is not None:
        chia.__version__ = version
    ssl = types.ModuleType("chia.ssl")
    create_ssl = types.ModuleType("chia.ssl.create_ssl")
    create_ssl.generate_ca_signed_cert = generate_cert
    chia.ssl = ssl
    ssl.create_ssl = create_ssl
    return {
        "yaml": yaml_module,
        "chia": chia,
        "chia.ssl": ssl,
        "chia.ssl.create_ssl": create_ssl,
    }


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
        self.output = self.root / "fullnode-ephemeral-test-connection"
        self.args = argparse.Namespace(
            chia_root=None, node_host=None, rpc_host=None, rpc_port=None,
            output=str(self.output),
        )

    def write_config(self):
        (self.root / "config/config.yaml").write_text(json.dumps(self.config))

    def run_export(self, generate=None, version="test-version"):
        self.write_config()

        def metadata_version(name):
            if isinstance(version, BaseException):
                raise version
            return version

        with patch.dict(os.environ, {"CHIA_ROOT": str(self.root)}), patch.object(
            exporter, "detect_host", return_value="192.168.1.25"
        ), patch.object(
            exporter.importlib.metadata, "version", side_effect=metadata_version
        ), patch.dict(sys.modules, {"chia": None}), contextlib.redirect_stdout(
            io.StringIO()
        ), contextlib.redirect_stderr(io.StringIO()):
            return exporter.export(self.args, json.loads, generate or fake_cert).resolve()

    def run_main(self, modules, argv, version="test-version"):
        def metadata_version(name):
            if isinstance(version, BaseException):
                raise version
            return version

        stderr = io.StringIO()
        with patch.object(
            exporter.importlib.metadata, "version", side_effect=metadata_version
        ), patch.dict(sys.modules, modules), contextlib.redirect_stdout(
            io.StringIO()
        ), contextlib.redirect_stderr(stderr):
            try:
                exporter.main(argv)
                code = None
            except SystemExit as error:
                code = error.code
        return code, stderr.getvalue()

    def argv(self, output=None):
        return [
            "--chia-root", str(self.root),
            "--node-host", "192.168.1.25",
            "--output", str(output or self.output),
        ]

    def test_defaults_preserve_network_and_export_only_leaf_keys(self):
        output = self.run_export()
        self.assertEqual(output.name, "fullnode-ephemeral-test-connection")
        manifest = (output / "connection.toml").read_text()
        self.assertIn('host = "192.168.1.25"', manifest)
        self.assertIn('port = 58445', manifest)
        self.assertIn('network = "ephemeral-test"', manifest)
        self.assertIn('[node.rpc]\nhost = "192.168.1.25"\nport = 8555', manifest)
        readme = (output / "README.txt").read_text()
        self.assertIn("RPC: 192.168.1.25:8555", readme)
        self.assertNotIn("ssh -N", readme)
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
        self.assertIn('[node.rpc]\nhost = "node.example.test"\nport = 8555',
                      (output / "connection.toml").read_text())
        original = (output / "client.key").read_bytes()
        with self.assertRaises(FileExistsError):
            self.run_export()
        self.assertEqual((output / "client.key").read_bytes(), original)

    def test_rpc_port_defaults_to_selected_nodes_configured_port(self):
        self.config["full_node"]["rpc_port"] = 58555
        output = self.run_export()
        self.assertIn('[node.rpc]\nhost = "192.168.1.25"\nport = 58555',
                      (output / "connection.toml").read_text())

    def test_explicit_rpc_endpoint_supports_tunnels_without_changing_peer(self):
        self.args.rpc_host = "127.0.0.1"
        self.args.rpc_port = 18555
        output = self.run_export()
        manifest = (output / "connection.toml").read_text()
        self.assertIn('[node]\nhost = "192.168.1.25"\nport = 58445', manifest)
        self.assertIn('[node.rpc]\nhost = "127.0.0.1"\nport = 18555', manifest)
        self.assertIn("RPC: 127.0.0.1:18555", (output / "README.txt").read_text())

    def test_invalid_rpc_endpoint_is_rejected_before_creating_bundle(self):
        for host in ("", " ", "0.0.0.0", "::"):
            self.args.rpc_host = host
            with self.assertRaisesRegex(ValueError, "--rpc-host"):
                self.run_export()
        self.args.rpc_host = None
        for rpc_port in (0, 65536, True):
            self.args.rpc_port = rpc_port
            with self.assertRaisesRegex(ValueError, "RPC port"):
                self.run_export()
        self.assertFalse(self.output.exists())

    def test_failed_generation_removes_partial_bundle(self):
        def fail(*args):
            raise ValueError("fixture signing failure")
        with self.assertRaisesRegex(ValueError, "fixture signing failure"):
            self.run_export(fail)
        self.assertFalse(self.output.exists())

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

    def test_missing_package_metadata_uses_unknown_version(self):
        output = self.run_export(
            version=importlib.metadata.PackageNotFoundError("chia-blockchain")
        )
        metadata = json.loads((output / "network.json").read_text())
        self.assertEqual(metadata["chia_version"], "unknown")

    def test_chia_module_version_preferred_over_metadata(self):
        modules = stub_modules(fake_cert, version="2.5.0-test")
        self.write_config()
        code, error = self.run_main(
            modules, self.argv(), version=importlib.metadata.PackageNotFoundError("chia-blockchain")
        )
        self.assertIsNone(code)
        metadata = json.loads((self.output / "network.json").read_text())
        self.assertEqual(metadata["chia_version"], "2.5.0-test")
        self.assertNotIn("Chia Python environment", error)

    def test_main_succeeds_without_package_metadata(self):
        self.write_config()
        code, error = self.run_main(
            stub_modules(fake_cert), self.argv(),
            version=importlib.metadata.PackageNotFoundError("chia-blockchain"),
        )
        self.assertIsNone(code)
        metadata = json.loads((self.output / "network.json").read_text())
        self.assertEqual(metadata["chia_version"], "unknown")
        self.assertNotIn("Chia Python environment", error)

    def test_import_error_inside_export_uses_export_message(self):
        def broken(*args):
            raise ImportError("fixture signing dependency")
        self.write_config()
        code, error = self.run_main(stub_modules(broken), self.argv())
        self.assertEqual(code, 1)
        self.assertIn("Export failed", error)
        self.assertIn("fixture signing dependency", error)
        self.assertNotIn("Chia Python environment", error)
        self.assertFalse(self.output.exists())

    def test_main_without_chia_or_yaml_reports_environment(self):
        self.write_config()
        yaml_module = stub_modules(fake_cert)["yaml"]
        for modules in ({"yaml": None}, {"yaml": yaml_module, "chia": None}):
            with self.subTest(modules=modules):
                code, error = self.run_main(modules, self.argv())
                self.assertEqual(code, 1)
                self.assertIn("Chia Python environment", error)

    def test_null_or_non_mapping_ca_sections_are_rejected(self):
        for name in ("chia_ssl_ca", "private_ssl_ca"):
            for value in (None, "config/ssl/ca", []):
                with self.subTest(name=name, value=value):
                    self.config[name] = value
                    with self.assertRaisesRegex(ValueError, f"{name} must be a mapping"):
                        self.run_export()
                    self.assertFalse(self.output.exists())
            self.config.pop(name, None)

    def test_missing_or_invalid_full_node_is_rejected(self):
        original = self.config["full_node"]
        for value in ("missing", None, [], "text"):
            with self.subTest(value=value):
                if value == "missing":
                    self.config.pop("full_node", None)
                    expected = "has no full_node section"
                else:
                    self.config["full_node"] = value
                    expected = (
                        "has no full_node section" if value is None
                        else "full_node must be a mapping"
                    )
                with self.assertRaisesRegex(ValueError, expected):
                    self.run_export()
                self.assertFalse(self.output.exists())
                self.config["full_node"] = original

    def test_relocated_ca_paths_are_source_of_exported_ca_bytes(self):
        relocated = self.root / "relocated-cas"
        relocated.mkdir()
        peer_crt = b"relocated peer public CA"
        rpc_crt = b"relocated private RPC CA"
        (relocated / "peer.crt").write_bytes(peer_crt)
        (relocated / "peer.key").write_bytes(b"relocated peer CA signing key")
        (relocated / "rpc.crt").write_bytes(rpc_crt)
        (relocated / "rpc.key").write_bytes(b"relocated RPC CA signing key")
        self.config["chia_ssl_ca"] = {
            "crt": "relocated-cas/peer.crt", "key": "relocated-cas/peer.key"
        }
        self.config["private_ssl_ca"] = {
            "crt": "relocated-cas/rpc.crt", "key": "relocated-cas/rpc.key"
        }
        output = self.run_export()
        self.assertEqual((output / "ca.crt").read_bytes(), peer_crt)
        self.assertEqual((output / "private-ca.crt").read_bytes(), rpc_crt)
        self.assertIn(peer_crt, (output / "client.crt").read_bytes())
        self.assertIn(rpc_crt, (output / "rpc-client.crt").read_bytes())

    def test_main_reports_actionable_config_error_without_traceback(self):
        self.config["chia_ssl_ca"] = None
        self.write_config()
        code, error = self.run_main(stub_modules(fake_cert), self.argv())
        self.assertEqual(code, 1)
        self.assertIn("chia_ssl_ca must be a mapping", error)
        self.assertNotIn("Chia Python environment", error)
        self.assertNotIn("Traceback", error)
        self.assertFalse(self.output.exists())

    def test_encrypted_ca_key_type_error_is_reported_as_export_failure(self):
        def encrypted(*args):
            raise TypeError("Password was not given but private key is encrypted")

        self.write_config()
        code, error = self.run_main(stub_modules(encrypted), self.argv())
        self.assertEqual(code, 1)
        self.assertIn("Export failed", error)
        self.assertIn("Password was not given", error)
        self.assertNotIn("Traceback", error)
        self.assertNotIn("Chia Python environment", error)
        self.assertFalse(self.output.exists())

    def test_non_json_serializable_override_is_reported_as_export_failure(self):
        from datetime import date

        self.write_config()
        self.config["network_overrides"]["constants"]["ephemeral-test"]["EXPORTED_AT"] = (
            date(2026, 1, 1)
        )
        modules = stub_modules(fake_cert)
        modules["yaml"].safe_load = lambda text: self.config
        code, error = self.run_main(modules, self.argv())
        self.assertEqual(code, 1)
        self.assertIn("Export failed", error)
        self.assertNotIn("Traceback", error)
        self.assertFalse(self.output.exists())

    def test_malformed_yaml_is_reported_as_export_failure(self):
        modules = stub_modules(fake_cert)

        def broken(text):
            raise modules["yaml"].YAMLError("fixture malformed yaml")

        modules["yaml"].safe_load = broken
        self.write_config()
        code, error = self.run_main(modules, self.argv())
        self.assertEqual(code, 1)
        self.assertIn("Export failed", error)
        self.assertIn("fixture malformed yaml", error)
        self.assertNotIn("Traceback", error)
        self.assertNotIn("Chia Python environment", error)
        self.assertFalse(self.output.exists())

    def test_empty_or_non_mapping_config_is_rejected(self):
        self.args.chia_root = str(self.root)
        config_path = self.root / "config/config.yaml"
        for text in ("null", "[]"):
            with self.subTest(text=text):
                config_path.write_text(text)
                with contextlib.redirect_stdout(
                    io.StringIO()
                ), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaisesRegex(
                        ValueError, "config.yaml must be a mapping"
                    ):
                        exporter.export(self.args, json.loads, fake_cert)
                self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
