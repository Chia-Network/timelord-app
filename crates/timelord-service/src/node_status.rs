//! Read-only private RPC, separate from the timelord peer session.
use anyhow::{Context, Result, bail};
use dg_xch_servers::transport::TlsIdentity;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use timelord_core::{NodeConfig, NodeRpcConfig};

#[derive(Debug, Serialize)]
pub struct NodeSyncStatus {
    synced: bool,
    syncing: bool,
    height: Option<u64>,
    target_height: Option<u64>,
    timestamp_unix: f64,
}
#[derive(Deserialize)]
struct Response {
    success: bool,
    blockchain_state: BlockchainState,
}
#[derive(Deserialize)]
struct BlockchainState {
    peak: Option<Peak>,
    sync: Sync,
}
#[derive(Deserialize)]
struct Peak {
    height: u64,
}
#[derive(Deserialize)]
struct Sync {
    synced: bool,
    sync_mode: bool,
    sync_tip_height: Option<u64>,
}
fn decode(bytes: &[u8]) -> Result<NodeSyncStatus> {
    let response: Response = serde_json::from_slice(bytes).context("Invalid full-node status")?;
    if !response.success {
        bail!("Full node rejected the status request");
    }
    let state = response.blockchain_state;
    Ok(NodeSyncStatus {
        synced: state.sync.synced,
        syncing: state.sync.sync_mode,
        height: state.peak.map(|p| p.height),
        target_height: state.sync.sync_tip_height.filter(|h| *h > 0),
        timestamp_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64(),
    })
}

pub async fn fetch(rpc: &NodeRpcConfig) -> Result<NodeSyncStatus> {
    crate::connection::validate_identity(&NodeConfig {
        certificate: Some(rpc.certificate.clone()),
        private_key: Some(rpc.private_key.clone()),
        ca_certificate: Some(rpc.ca_certificate.clone()),
        ..NodeConfig::default()
    })?;
    let tls = TlsIdentity {
        certificate: rpc.certificate.clone(),
        private_key: rpc.private_key.clone(),
        ca_certificate: rpc.ca_certificate.clone(),
    }
    .client()?;
    // Connect to the configured address while verifying Chia's TLS server name.
    // No public trust roots, proxy routing, redirects or disabled verification.
    rustls::pki_types::ServerName::try_from(rpc.server_name.clone())?;
    let addresses: Vec<_> = tokio::net::lookup_host((rpc.host.as_str(), rpc.port))
        .await?
        .collect();
    if addresses.is_empty() {
        bail!("Full-node status host has no addresses");
    }
    let url = format!(
        "https://{}:{}/get_blockchain_state",
        rpc.server_name, rpc.port
    );
    let client = reqwest::Client::builder()
        .use_preconfigured_tls((*tls).clone())
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(&rpc.server_name, &addresses)
        .timeout(Duration::from_secs(4))
        .build()?;
    let mut response = client
        .post(url)
        .json(&serde_json::json!({}))
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
            bail!("Full-node status response too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sync_is_reported_from_rpc_not_inferred_from_peak_or_connection() {
        let status = decode(br#"{"success":true,"blockchain_state":{"peak":{"height":900},"sync":{"synced":false,"sync_mode":true,"sync_tip_height":1000}}}"#).unwrap();
        assert!(!status.synced);
        assert!(status.syncing);
        assert_eq!(status.height, Some(900));
        assert_eq!(status.target_height, Some(1000));
        let empty = decode(br#"{"success":true,"blockchain_state":{"peak":null,"sync":{"synced":false,"sync_mode":false,"sync_tip_height":0}}}"#).unwrap();
        assert_eq!(empty.height, None);
        assert!(!empty.synced);
        assert!(
            decode(br#"{"success":true,"blockchain_state":{"peak":{"height":900},"sync":{}}}"#)
                .is_err()
        );
    }

    #[tokio::test]
    async fn status_requires_the_configured_ca_and_valid_server_name() {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let directory =
            std::env::temp_dir().join(format!("timelord-rpc-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let ca_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["fixture-ca".into()]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["node.fixture".into()])
            .unwrap()
            .signed_by(&key, &ca, &ca_key)
            .unwrap();
        std::fs::write(directory.join("ca.crt"), ca.pem()).unwrap();
        std::fs::write(directory.join("leaf.crt"), leaf.pem()).unwrap();
        std::fs::write(directory.join("leaf.key"), key.serialize_pem()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                directory.join("leaf.key"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .unwrap();
        let server = rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
            for _ in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                if let Ok(mut tls) = acceptor.accept(socket).await {
                    assert!(!tls.get_ref().1.peer_certificates().unwrap().is_empty());
                    let mut request = vec![0; 4096];
                    let n = tls.read(&mut request).await.unwrap();
                    assert!(request[..n].starts_with(b"POST /get_blockchain_state HTTP/1.1"));
                    let body = r#"{"success":true,"blockchain_state":{"peak":{"height":123},"sync":{"synced":true,"sync_mode":false}}}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    tls.write_all(response.as_bytes()).await.unwrap();
                    let _ = tls.shutdown().await;
                }
            }
        });
        let mut rpc = NodeRpcConfig {
            host: "127.0.0.1".into(),
            port,
            server_name: "node.fixture".into(),
            certificate: directory.join("leaf.crt"),
            private_key: directory.join("leaf.key"),
            ca_certificate: directory.join("ca.crt"),
        };
        let status = fetch(&rpc).await.unwrap();
        assert!(status.synced);
        assert_eq!(status.height, Some(123));
        rpc.server_name = "wrong.fixture".into();
        assert!(fetch(&rpc).await.is_err());
        rpc.server_name = "node.fixture".into();
        let other_key = KeyPair::generate().unwrap();
        let mut other_params = CertificateParams::new(vec!["another-ca".into()]).unwrap();
        other_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let other_ca = other_params.self_signed(&other_key).unwrap();
        std::fs::write(directory.join("another-ca.crt"), other_ca.pem()).unwrap();
        rpc.ca_certificate = directory.join("another-ca.crt");
        assert!(fetch(&rpc).await.is_err());
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
