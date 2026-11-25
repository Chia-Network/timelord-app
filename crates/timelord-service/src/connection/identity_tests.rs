use super::{local, validate_identity, validate_peer_identity};
use crate::test_pki;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use timelord_core::{Config, NodeConfig, NodeRpcConfig};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "timelord-identity-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        self.write_under(Path::new(name), bytes)
    }

    fn write_under(&self, relative: &Path, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn client_paths(
    fixture: &Fixture,
    label: &str,
    ca: &[u8],
    leaf: &[u8],
    key: &[u8],
) -> (PathBuf, PathBuf, PathBuf) {
    (
        fixture.write(&format!("{label}.crt"), leaf),
        fixture.write(&format!("{label}.key"), key),
        fixture.write(&format!("{label}-ca.crt"), ca),
    )
}

fn peer_node(fixture: &Fixture, label: &str, ca: &[u8], leaf: &[u8], key: &[u8]) -> NodeConfig {
    let (certificate, private_key, ca_certificate) = client_paths(fixture, label, ca, leaf, key);
    NodeConfig {
        certificate: Some(certificate),
        private_key: Some(private_key),
        ca_certificate: Some(ca_certificate),
        ..NodeConfig::default()
    }
}

fn rpc_config(fixture: &Fixture, label: &str, ca: &[u8], leaf: &[u8], key: &[u8]) -> NodeRpcConfig {
    let (certificate, private_key, ca_certificate) = client_paths(fixture, label, ca, leaf, key);
    NodeRpcConfig {
        host: "localhost".into(),
        port: 8555,
        server_name: "chia.net".into(),
        certificate,
        private_key,
        ca_certificate,
    }
}

#[test]
fn peer_leaf_must_be_issued_by_the_configured_ca() {
    let fixture = Fixture::new();
    let (issuer, ca_pem, _) = test_pki::ca("peer-ca");
    let (_, unrelated_ca_pem, _) = test_pki::ca("unrelated-ca");
    let (leaf_pem, leaf_key_pem) = test_pki::leaf(&issuer, &["chia.net"]);
    let mut node = peer_node(&fixture, "peer", &ca_pem, &leaf_pem, &leaf_key_pem);
    validate_identity(&node).unwrap();

    node.ca_certificate = Some(fixture.write("unrelated-ca.crt", &unrelated_ca_pem));
    let error = validate_identity(&node).unwrap_err().to_string();
    assert!(
        error.contains("failed validation against the configured CA"),
        "unexpected error: {error}"
    );
}

#[test]
fn peer_leaf_may_chain_through_an_intermediate_ca() {
    let fixture = Fixture::new();
    let (root_issuer, root_ca_pem, _) = test_pki::ca("root-ca");
    let (intermediate_issuer, intermediate_pem) =
        test_pki::intermediate(&root_issuer, "intermediate-ca");
    let (leaf_pem, leaf_key_pem) = test_pki::leaf(&intermediate_issuer, &["chia.net"]);
    let mut chain = leaf_pem;
    chain.extend_from_slice(&intermediate_pem);
    let node = peer_node(&fixture, "peer", &root_ca_pem, &chain, &leaf_key_pem);
    validate_identity(&node).unwrap();
}

#[test]
fn rpc_identity_is_validated_alongside_the_peer() {
    let fixture = Fixture::new();
    let (peer_issuer, peer_ca_pem, _) = test_pki::ca("peer-ca");
    let (peer_leaf_pem, peer_key_pem) = test_pki::leaf(&peer_issuer, &["chia.net"]);
    let (rpc_issuer, rpc_ca_pem, _) = test_pki::ca("rpc-ca");
    let (rpc_leaf_pem, rpc_key_pem) = test_pki::leaf(&rpc_issuer, &["chia.net"]);
    let mut node = peer_node(
        &fixture,
        "peer",
        &peer_ca_pem,
        &peer_leaf_pem,
        &peer_key_pem,
    );
    node.rpc = Some(rpc_config(
        &fixture,
        "rpc",
        &rpc_ca_pem,
        &rpc_leaf_pem,
        &rpc_key_pem,
    ));
    validate_identity(&node).unwrap();
}

#[test]
fn rpc_leaf_must_be_issued_by_the_rpc_ca() {
    let fixture = Fixture::new();
    let (peer_issuer, peer_ca_pem, _) = test_pki::ca("peer-ca");
    let (peer_leaf_pem, peer_key_pem) = test_pki::leaf(&peer_issuer, &["chia.net"]);
    let (rpc_issuer, _, _) = test_pki::ca("rpc-ca");
    let (_, unrelated_ca_pem, _) = test_pki::ca("unrelated-ca");
    let (rpc_leaf_pem, rpc_key_pem) = test_pki::leaf(&rpc_issuer, &["chia.net"]);
    let mut node = peer_node(
        &fixture,
        "peer",
        &peer_ca_pem,
        &peer_leaf_pem,
        &peer_key_pem,
    );
    node.rpc = Some(rpc_config(
        &fixture,
        "rpc",
        &unrelated_ca_pem,
        &rpc_leaf_pem,
        &rpc_key_pem,
    ));
    let error = format!("{:#}", validate_identity(&node).unwrap_err());
    assert!(
        error.starts_with("Invalid RPC connection identity"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("failed validation against the configured CA"),
        "unexpected error: {error}"
    );
}

#[test]
fn peer_validation_ignores_a_broken_rpc_identity() {
    let fixture = Fixture::new();
    let (peer_issuer, peer_ca_pem, _) = test_pki::ca("peer-ca");
    let (peer_leaf_pem, peer_key_pem) = test_pki::leaf(&peer_issuer, &["chia.net"]);
    let (rpc_issuer, _, _) = test_pki::ca("rpc-ca");
    let (_, unrelated_ca_pem, _) = test_pki::ca("unrelated-ca");
    let (rpc_leaf_pem, rpc_key_pem) = test_pki::leaf(&rpc_issuer, &["chia.net"]);
    let mut node = peer_node(
        &fixture,
        "peer",
        &peer_ca_pem,
        &peer_leaf_pem,
        &peer_key_pem,
    );
    node.rpc = Some(rpc_config(
        &fixture,
        "rpc",
        &unrelated_ca_pem,
        &rpc_leaf_pem,
        &rpc_key_pem,
    ));

    validate_peer_identity(&node).unwrap();
    assert!(validate_identity(&node).is_err());
}

#[test]
fn local_root_keeps_only_a_valid_rpc_identity() {
    let fixture = Fixture::new();
    let (peer_issuer, peer_ca_pem, _) = test_pki::ca("chia_ca");
    let (peer_leaf_pem, peer_key_pem) = test_pki::leaf(&peer_issuer, &["chia.net"]);
    let (rpc_issuer, rpc_ca_pem, _) = test_pki::ca("private_ca");
    let (rpc_leaf_pem, rpc_key_pem) = test_pki::leaf(&rpc_issuer, &["chia.net"]);
    let files: [(&str, &[u8]); 7] = [
        (
            "chia/config/ssl/timelord/public_timelord.crt",
            &peer_leaf_pem,
        ),
        (
            "chia/config/ssl/timelord/public_timelord.key",
            &peer_key_pem,
        ),
        ("chia/config/ssl/ca/chia_ca.crt", &peer_ca_pem),
        (
            "chia/config/ssl/full_node/private_full_node.crt",
            &rpc_leaf_pem,
        ),
        (
            "chia/config/ssl/full_node/private_full_node.key",
            &rpc_key_pem,
        ),
        ("chia/config/ssl/ca/private_ca.crt", &rpc_ca_pem),
        (
            "chia/config/config.yaml",
            br#"full_node:
  rpc_port: 8556
"#,
        ),
    ];
    for (path, bytes) in files {
        fixture.write_under(Path::new(path), bytes);
    }
    let root = fixture.0.join("chia");
    let mut config = Config::default();
    local(&mut config, &root).unwrap();
    assert!(config.node().rpc.is_some());

    let (_, unrelated_ca_pem, _) = test_pki::ca("unrelated-ca");
    fixture.write_under(
        Path::new("chia/config/ssl/ca/private_ca.crt"),
        &unrelated_ca_pem,
    );
    let mut config = Config::default();
    local(&mut config, &root).unwrap();
    assert!(config.node().rpc.is_none());
    assert!(config.node().certificate.is_some());
}

#[test]
fn rpc_identity_rejects_ca_certificates_and_ca_keys() {
    let fixture = Fixture::new();
    let (peer_issuer, peer_ca_pem, _) = test_pki::ca("peer-ca");
    let (peer_leaf_pem, peer_key_pem) = test_pki::leaf(&peer_issuer, &["chia.net"]);
    let (rpc_issuer, rpc_ca_pem, rpc_ca_key_pem) = test_pki::ca("rpc-ca");
    let mut node = peer_node(
        &fixture,
        "peer",
        &peer_ca_pem,
        &peer_leaf_pem,
        &peer_key_pem,
    );

    node.rpc = Some(rpc_config(
        &fixture,
        "rpc-ca-leaf",
        &rpc_ca_pem,
        &rpc_ca_pem,
        &rpc_ca_key_pem,
    ));
    let error = format!("{:#}", validate_identity(&node).unwrap_err());
    assert!(
        error.contains("not a CA signing identity"),
        "unexpected error: {error}"
    );

    let (key_reuse_leaf, key_reuse_key) =
        test_pki::leaf_with_key(&rpc_issuer, rpc_issuer.key(), &["chia.net"]);
    node.rpc = Some(rpc_config(
        &fixture,
        "rpc-key-reuse",
        &rpc_ca_pem,
        &key_reuse_leaf,
        &key_reuse_key,
    ));
    let error = format!("{:#}", validate_identity(&node).unwrap_err());
    assert!(
        error.contains("own key, not the CA signing key"),
        "unexpected error: {error}"
    );
}
