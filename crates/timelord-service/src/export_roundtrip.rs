//! Runs the real Python exporter and imports its output through the Rust importer.
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const NETWORK: &str = "ephemeral-roundtrip";

const CREATE_SSL_STUB: &str = r#"import subprocess
import tempfile
from pathlib import Path


def as_bytes(value):
    return value if isinstance(value, (bytes, bytearray)) else Path(value).read_bytes()


def generate_ca_signed_cert(ca_crt, ca_key, crt_path, key_path):
    crt_path = Path(crt_path)
    key_path = Path(key_path)
    with tempfile.TemporaryDirectory() as temporary:
        temporary = Path(temporary)
        ca_crt_path = temporary / "ca.crt"
        ca_key_path = temporary / "ca.key"
        ca_crt_path.write_bytes(as_bytes(ca_crt))
        ca_key_path.write_bytes(as_bytes(ca_key))
        csr = temporary / "leaf.csr"
        serial = temporary / "ca.srl"
        subprocess.run(
            ["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes",
             "-keyout", str(key_path), "-out", str(csr),
             "-subj", "/CN=timelord-export-test"],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        subprocess.run(
            ["openssl", "x509", "-req", "-in", str(csr),
             "-CA", str(ca_crt_path), "-CAkey", str(ca_key_path),
             "-CAserial", str(serial), "-CAcreateserial",
             "-out", str(crt_path), "-days", "1", "-sha256"],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
"#;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "timelord-export-roundtrip-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_ca(directory: &Path, name: &str) -> (Vec<u8>, Vec<u8>) {
    let mut params = CertificateParams::new(vec![name.into()]).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let certificate_pem = certificate.pem().into_bytes();
    let key_pem = key.serialize_pem().into_bytes();
    fs::write(directory.join(format!("{name}.crt")), &certificate_pem).unwrap();
    fs::write(directory.join(format!("{name}.key")), &key_pem).unwrap();
    (certificate_pem, key_pem)
}

fn write_stubs(directory: &Path) {
    fs::create_dir_all(directory.join("chia/ssl")).unwrap();
    fs::write(
        directory.join("yaml.py"),
        "import json\n\ndef safe_load(text):\n    return json.loads(text)\n",
    )
    .unwrap();
    fs::write(
        directory.join("chia/__init__.py"),
        "__version__ = \"2.5.0-test\"\n",
    )
    .unwrap();
    fs::write(directory.join("chia/ssl/__init__.py"), "").unwrap();
    fs::write(directory.join("chia/ssl/create_ssl.py"), CREATE_SSL_STUB).unwrap();
}

fn exporter_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/export-fullnode-connection")
}

#[test]
fn exported_bundle_imports_with_matching_network_credentials_and_provenance() {
    let temp = TempDir::new();
    let chia_root = temp.path().join("chia-root");
    let ca_directory = chia_root.join("config/ssl/ca");
    fs::create_dir_all(&ca_directory).unwrap();
    let (peer_ca, peer_ca_key) = write_ca(&ca_directory, "chia_ca");
    let (rpc_ca, rpc_ca_key) = write_ca(&ca_directory, "private_ca");
    let genesis = "ab".repeat(32);
    let config_yaml = format!(
        "{{\n  \"selected_network\": \"{NETWORK}\",\n  \"full_node\": {{\"port\": 58445, \"rpc_port\": 8555}},\n  \"network_overrides\": {{\"constants\": {{\"{NETWORK}\": {{\"GENESIS_CHALLENGE\": \"{genesis}\", \"DIFFICULTY_STARTING\": 250}}}}}}\n}}\n"
    );
    fs::write(chia_root.join("config/config.yaml"), config_yaml).unwrap();

    let stubs = temp.path().join("stubs");
    write_stubs(&stubs);
    let bundle = temp.path().join("bundle");
    let output = Command::new("python3")
        .arg(exporter_path())
        .args(["--chia-root"])
        .arg(&chia_root)
        .args(["--node-host", "node.example.test"])
        .args(["--rpc-host", "rpc.example.test"])
        .args(["--output"])
        .arg(&bundle)
        .env("PYTHONPATH", &stubs)
        .env_remove("CHIA_ROOT")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("python3 is required for the exporter round-trip test");
    assert!(
        output.status.success(),
        "exporter failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for name in [
        "connection.toml",
        "client.crt",
        "client.key",
        "ca.crt",
        "rpc-client.crt",
        "rpc-client.key",
        "private-ca.crt",
        "network.json",
        "README.txt",
    ] {
        assert!(bundle.join(name).is_file(), "exporter did not write {name}");
    }
    let exported = |name: &str| fs::read(bundle.join(name)).unwrap();
    assert_eq!(exported("ca.crt"), peer_ca);
    assert_eq!(exported("private-ca.crt"), rpc_ca);
    assert_ne!(exported("client.crt"), peer_ca);
    assert_ne!(exported("client.key"), peer_ca_key);
    assert_ne!(exported("rpc-client.crt"), rpc_ca);
    assert_ne!(exported("rpc-client.key"), rpc_ca_key);

    let config_path = temp.path().join("app/config.toml");
    crate::save(&config_path, &timelord_core::Config::default()).unwrap();
    crate::connection::import(&config_path, &bundle.join("connection.toml")).unwrap();
    let config = crate::load(&config_path).unwrap();

    assert_eq!(config.node.network.label(), NETWORK);
    assert_eq!(config.node.host, "node.example.test");
    assert_eq!(config.node.port, 58445);
    let certificate = config.node.certificate.as_ref().unwrap();
    let private_key = config.node.private_key.as_ref().unwrap();
    let ca_certificate = config.node.ca_certificate.as_ref().unwrap();
    let identities = config_path.parent().unwrap().join("identities");
    assert!(certificate.starts_with(&identities));
    assert_eq!(certificate.file_name().unwrap(), "client.crt");
    assert_eq!(private_key.file_name().unwrap(), "client.key");
    assert_eq!(ca_certificate.file_name().unwrap(), "ca.crt");
    assert_eq!(fs::read(certificate).unwrap(), exported("client.crt"));
    assert_eq!(fs::read(private_key).unwrap(), exported("client.key"));
    assert_eq!(fs::read(ca_certificate).unwrap(), exported("ca.crt"));

    let rpc = config.node.rpc.as_ref().unwrap();
    assert_eq!(rpc.host, "rpc.example.test");
    assert_eq!(rpc.port, 8555);
    assert_eq!(rpc.certificate.file_name().unwrap(), "rpc.crt");
    assert_eq!(rpc.private_key.file_name().unwrap(), "rpc.key");
    assert_eq!(rpc.ca_certificate.file_name().unwrap(), "rpc-ca.crt");
    assert_eq!(
        fs::read(&rpc.certificate).unwrap(),
        exported("rpc-client.crt")
    );
    assert_eq!(
        fs::read(&rpc.private_key).unwrap(),
        exported("rpc-client.key")
    );
    assert_eq!(
        fs::read(&rpc.ca_certificate).unwrap(),
        exported("private-ca.crt")
    );

    let definition = config.node.network_definition.as_ref().unwrap();
    assert_eq!(definition.schema_version, 1);
    assert_eq!(definition.network_id, NETWORK);
    assert_eq!(definition.chia_version, "2.5.0-test");
    assert!(!definition.exported_at.is_empty());
    assert_eq!(
        definition
            .constants
            .get("GENESIS_CHALLENGE")
            .and_then(|value| value.as_str()),
        Some(genesis.as_str())
    );
    assert_eq!(
        definition
            .constants
            .get("DIFFICULTY_STARTING")
            .unwrap()
            .unsigned::<u64>()
            .unwrap(),
        250
    );
    let resolved = crate::network::resolve(&config.node).unwrap();
    assert_eq!(resolved.network_id, NETWORK);
    assert_eq!(resolved.constants.difficulty_starting, 250);
}
