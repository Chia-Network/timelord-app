//! Import dedicated connection identities, never CA signing keys.
use anyhow::{Context, Result, bail};
use dg_xch_servers::transport::TlsIdentity;
use serde::Deserialize;
use std::{fs, path::Path};
use timelord_core::{Config, NodeConfig};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    schema_version: u32,
    node: NodeConfig,
}
pub fn local(config: &mut Config, root: &Path) -> Result<()> {
    // Chia's full-node peer port uses its public TLS context. A timelord is
    // a public client; the installation's private CA belongs to private services.
    let ssl = root.join("config/ssl");
    let mut node = config.node.clone();
    node.host = "localhost".into();
    node.network_definition = None;
    node.server_name = Some("chia.net".into());
    node.certificate = Some(
        ssl.join("timelord/public_timelord.crt")
            .canonicalize()
            .context("Timelord certificate not found in this Chia folder")?,
    );
    node.private_key = Some(
        ssl.join("timelord/public_timelord.key")
            .canonicalize()
            .context("Timelord private key not found")?,
    );
    node.ca_certificate = Some(
        ssl.join("ca/chia_ca.crt")
            .canonicalize()
            .context("Node CA certificate not found")?,
    );
    validate_identity(&node)?;
    // The peer identity cannot authenticate private RPC. Use the installation's
    // existing RPC leaf; never read a CA signing key.
    node.rpc = None;
    let yaml_path = root.join("config/config.yaml");
    if yaml_path.exists() {
        let documents = yaml_rust2::YamlLoader::load_from_str(&fs::read_to_string(yaml_path)?)?;
        let yaml = documents.first().context("Chia configuration is empty")?;
        let full_node = &yaml["full_node"];
        if let Some(name) = full_node["selected_network"]
            .as_str()
            .or(yaml["selected_network"].as_str())
        {
            if yaml["selected_network"]
                .as_str()
                .is_some_and(|root| root != name)
            {
                bail!("Full node and root selected_network disagree");
            }
            node.network = name.parse().map_err(anyhow::Error::msg)?;
            if matches!(node.network, timelord_core::Network::Custom(_)) {
                let source = if !full_node["network_overrides"].is_badvalue() {
                    &full_node["network_overrides"]
                } else {
                    &yaml["network_overrides"]
                };
                node.network_definition = Some(timelord_core::NetworkDefinition {
                    schema_version: 1,
                    network_id: name.into(),
                    exported_at: "local-root".into(),
                    chia_version: "unknown (local root)".into(),
                    constants: crate::network::yaml_overrides(&source["constants"][name])?,
                    config_overrides: crate::network::yaml_overrides(&source["config"][name])?,
                });
            }
        }
        if let Some(port) = full_node["port"].as_i64() {
            node.port = u16::try_from(port)
                .ok()
                .filter(|p| *p > 0)
                .context("Invalid full-node peer port")?;
        }
        if let Some(port) = full_node["rpc_port"]
            .as_i64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
        {
            let certificate = root.join(
                full_node["ssl"]["private_crt"]
                    .as_str()
                    .unwrap_or("config/ssl/full_node/private_full_node.crt"),
            );
            let private_key = root.join(
                full_node["ssl"]["private_key"]
                    .as_str()
                    .unwrap_or("config/ssl/full_node/private_full_node.key"),
            );
            let ca_certificate = root.join(
                yaml["private_ssl_ca"]["crt"]
                    .as_str()
                    .unwrap_or("config/ssl/ca/private_ca.crt"),
            );
            if certificate.is_file() && private_key.is_file() && ca_certificate.is_file() {
                node.rpc = Some(timelord_core::NodeRpcConfig {
                    host: "localhost".into(),
                    port,
                    server_name: "chia.net".into(),
                    certificate: certificate.canonicalize()?,
                    private_key: private_key.canonicalize()?,
                    ca_certificate: ca_certificate.canonicalize()?,
                });
            }
        }
    }
    let mut candidate = config.clone();
    candidate.add_connection(node).map_err(anyhow::Error::msg)?;
    candidate.validate().map_err(anyhow::Error::msg)?;
    crate::network::resolve(&candidate.node)?;
    *config = candidate;
    Ok(())
}
pub fn validate_identity(node: &NodeConfig) -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let certificate = node
        .certificate
        .clone()
        .context("Connection bundle has no client certificate")?;
    let key = node
        .private_key
        .clone()
        .context("Connection bundle has no client private key")?;
    let ca = node
        .ca_certificate
        .clone()
        .context("Connection bundle has no CA certificate")?;
    // Inspect the certificate, rather than a filename or PEM formatting.
    use x509_cert::der::Decode;
    let certificates =
        dg_xch_core::ssl::load_certs(certificate.to_str().context("Invalid certificate path")?)?;
    let leaf = certificates
        .first()
        .context("Client certificate is empty")?;
    let parsed = x509_cert::Certificate::from_der(leaf.as_ref())?;
    for extension in parsed.tbs_certificate.extensions.iter().flatten() {
        if extension.extn_id.to_string() == "2.5.29.19"
            && x509_cert::ext::pkix::BasicConstraints::from_der(extension.extn_value.as_bytes())?.ca
        {
            bail!("Use a dedicated client identity, not a CA signing identity");
        }
    }
    for root in dg_xch_core::ssl::load_certs(ca.to_str().context("Invalid CA certificate path")?)? {
        let root = x509_cert::Certificate::from_der(root.as_ref())?;
        if root.tbs_certificate.subject_public_key_info
            == parsed.tbs_certificate.subject_public_key_info
        {
            bail!("The client must use its own key, not the CA signing key");
        }
    }
    TlsIdentity {
        certificate,
        private_key: key,
        ca_certificate: ca,
    }
    .client()
    .context("Invalid connection identity")?;
    Ok(())
}
fn bundled_path(directory: &Path, path: Option<&std::path::PathBuf>) -> Result<std::path::PathBuf> {
    let path = path.context("Incomplete connection bundle")?;
    if path.is_absolute() {
        bail!("Bundle credential paths must be relative to the bundle folder");
    }
    let resolved = directory
        .join(path)
        .canonicalize()
        .context("Bundle credential file is missing")?;
    if !resolved.starts_with(directory.canonicalize()?) || !resolved.is_file() {
        bail!("Credential paths must stay inside the connection bundle");
    }
    Ok(resolved)
}
pub fn import(config_path: &Path, manifest: &Path) -> Result<()> {
    let bundle: Bundle = toml::from_str(&fs::read_to_string(manifest)?)?;
    if bundle.schema_version != 1 {
        bail!("Unsupported connection bundle version");
    }
    let mut config = crate::load(config_path)?;
    let folder = manifest.parent().context("Bundle folder missing")?;
    let mut node = bundle.node;
    if matches!(node.network, timelord_core::Network::Custom(_)) {
        let path = bundled_path(folder, Some(&std::path::PathBuf::from("network.json")))?;
        let definition =
            serde_json::from_str(&crate::config_checkpoint::bounded_text(&path, false)?)
                .context("Reading network.json")?;
        node.network_definition = Some(definition);
    }
    node.validate().map_err(anyhow::Error::msg)?;
    crate::network::resolve(&node)?;
    node.certificate = Some(bundled_path(folder, node.certificate.as_ref())?);
    node.private_key = Some(bundled_path(folder, node.private_key.as_ref())?);
    node.ca_certificate = Some(bundled_path(folder, node.ca_certificate.as_ref())?);
    if let Some(rpc) = &mut node.rpc {
        rpc.certificate = bundled_path(folder, Some(&rpc.certificate))?;
        rpc.private_key = bundled_path(folder, Some(&rpc.private_key))?;
        rpc.ca_certificate = bundled_path(folder, Some(&rpc.ca_certificate))?;
    }
    validate_identity(&node)?;
    let parent = config_path
        .parent()
        .context("Configuration folder missing")?;
    let identities = parent.join("identities");
    fs::create_dir_all(&identities)?;
    let directory = identities.join(format!(
        "node-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let result = (|| -> Result<()> {
        for (source, name) in [
            (&mut node.certificate, "client.crt"),
            (&mut node.private_key, "client.key"),
            (&mut node.ca_certificate, "ca.crt"),
        ] {
            let destination = directory.join(name);
            fs::copy(source.as_ref().context("Missing credential")?, &destination)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))?;
            }
            *source = Some(destination);
        }
        if let Some(rpc) = &mut node.rpc {
            for (source, name) in [
                (&mut rpc.certificate, "rpc.crt"),
                (&mut rpc.private_key, "rpc.key"),
                (&mut rpc.ca_certificate, "rpc-ca.crt"),
            ] {
                let destination = directory.join(name);
                fs::copy(&*source, &destination)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))?;
                }
                *source = destination;
            }
        }
        validate_identity(&node)?;
        config.add_connection(node).map_err(anyhow::Error::msg)?;
        crate::save(config_path, &config)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&directory);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    #[test]
    fn imports_a_dedicated_identity_and_rejects_ca_keys_and_escaping_paths_without_replacing_config()
     {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("timelord-import-fixture-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("bundle");
        fs::create_dir(&source).unwrap();
        let mut params = CertificateParams::new(vec!["fixture-ca".into()]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&ca_key).unwrap();
        let client_key = KeyPair::generate().unwrap();
        let client = CertificateParams::new(vec!["fixture-client".into()])
            .unwrap()
            .signed_by(&client_key, &ca, &ca_key)
            .unwrap();
        fs::write(source.join("client.crt"), client.pem()).unwrap();
        fs::write(source.join("client.key"), client_key.serialize_pem()).unwrap();
        fs::set_permissions(source.join("client.key"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(source.join("ca.crt"), ca.pem()).unwrap();
        fs::write(source.join("ca.key"), ca_key.serialize_pem()).unwrap();
        fs::set_permissions(source.join("ca.key"), fs::Permissions::from_mode(0o600)).unwrap();
        let chia_root = directory.join("chia-root");
        let ssl = chia_root.join("config/ssl");
        fs::create_dir_all(ssl.join("timelord")).unwrap();
        fs::create_dir_all(ssl.join("ca")).unwrap();
        fs::copy(
            source.join("client.crt"),
            ssl.join("timelord/public_timelord.crt"),
        )
        .unwrap();
        fs::copy(
            source.join("client.key"),
            ssl.join("timelord/public_timelord.key"),
        )
        .unwrap();
        fs::copy(source.join("ca.crt"), ssl.join("ca/chia_ca.crt")).unwrap();
        fs::create_dir_all(ssl.join("full_node")).unwrap();
        fs::copy(
            source.join("client.crt"),
            ssl.join("full_node/private_full_node.crt"),
        )
        .unwrap();
        fs::copy(
            source.join("client.key"),
            ssl.join("full_node/private_full_node.key"),
        )
        .unwrap();
        fs::copy(source.join("ca.crt"), ssl.join("ca/private_ca.crt")).unwrap();
        fs::write(
            chia_root.join("config/config.yaml"),
            "full_node:\n  rpc_port: 8556\n",
        )
        .unwrap();
        let mut local_config = Config::default();
        local(&mut local_config, &chia_root).unwrap();
        assert!(
            local_config
                .node
                .certificate
                .unwrap()
                .ends_with("public_timelord.crt")
        );
        assert!(
            local_config
                .node
                .ca_certificate
                .unwrap()
                .ends_with("chia_ca.crt")
        );
        let rpc = local_config.node.rpc.unwrap();
        assert_eq!(rpc.port, 8556);
        assert!(rpc.certificate.ends_with("full_node/private_full_node.crt"));
        assert!(rpc.ca_certificate.ends_with("ca/private_ca.crt"));
        fs::write(chia_root.join("config/config.yaml"), format!(
            "selected_network: ephemeral-fixture\nfull_node:\n  selected_network: ephemeral-fixture\n  port: 58445\nnetwork_overrides:\n  constants:\n    ephemeral-fixture:\n      GENESIS_CHALLENGE: '{}'\n      DIFFICULTY_STARTING: 250\n",
            "ab".repeat(32)
        )).unwrap();
        let mut custom_local = Config::default();
        local(&mut custom_local, &chia_root).unwrap();
        assert_eq!(custom_local.node.network.label(), "ephemeral-fixture");
        assert_eq!(custom_local.node.port, 58445);
        assert_eq!(
            crate::network::resolve(&custom_local.node)
                .unwrap()
                .constants
                .difficulty_starting,
            250
        );
        let manifest = source.join("connection.toml");
        let text = "schema_version=1\n[node]\nhost='fixture.invalid'\ncertificate='client.crt'\nprivate_key='client.key'\nca_certificate='ca.crt'\n";
        fs::write(&manifest, text).unwrap();
        let config_path = directory.join("config.toml");
        crate::save(&config_path, &Config::default()).unwrap();
        import(&config_path, &manifest).unwrap();
        let config = crate::load(&config_path).unwrap();
        assert_eq!(config.node.host, "fixture.invalid");
        let mainnet_id = config.connections.active_id.clone().unwrap();
        let mainnet_node = config.node.clone();
        assert!(
            config
                .node
                .private_key
                .unwrap()
                .starts_with(directory.join("identities"))
        );
        let saved = fs::read(&config_path).unwrap();
        fs::write(
            &manifest,
            text.replace("certificate='client.crt'", "certificate='ca.crt'")
                .replace("private_key='client.key'", "private_key='ca.key'"),
        )
        .unwrap();
        assert!(import(&config_path, &manifest).is_err());
        fs::write(directory.join("outside.key"), "fixture").unwrap();
        fs::write(
            &manifest,
            text.replace("private_key='client.key'", "private_key='../outside.key'"),
        )
        .unwrap();
        assert!(import(&config_path, &manifest).is_err());
        assert_eq!(fs::read(&config_path).unwrap(), saved);
        assert_eq!(
            fs::read_dir(directory.join("identities")).unwrap().count(),
            1
        );
        // Use only the generated fixture identities for the custom import.
        let custom = crate::network::tests::node();
        let custom_text = text.replace(
            "host='fixture.invalid'",
            "host='fixture.invalid'\nport=58445\nnetwork='ephemeral-fixture'",
        );
        fs::write(&manifest, &custom_text).unwrap();
        let metadata = source.join("network.json");
        // A missing/mismatched or escaping definition cannot change saved settings.
        assert!(import(&config_path, &manifest).is_err());
        let mut invalid = custom.network_definition.clone().unwrap();
        invalid.network_id = "different".into();
        fs::write(&metadata, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(import(&config_path, &manifest).is_err());
        let mut invalid = custom.network_definition.clone().unwrap();
        invalid
            .constants
            .insert("NUM_SPS_SUB_SLOT".into(), 0.into());
        fs::write(&metadata, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(import(&config_path, &manifest).is_err());
        assert_eq!(fs::read(&config_path).unwrap(), saved);
        fs::remove_file(&metadata).unwrap();
        let outside = directory.join("outside.json");
        fs::write(
            &outside,
            serde_json::to_vec(&custom.network_definition).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, &metadata).unwrap();
        assert!(import(&config_path, &manifest).is_err());
        fs::remove_file(&metadata).unwrap();
        fs::write(
            &metadata,
            serde_json::to_vec(&custom.network_definition).unwrap(),
        )
        .unwrap();
        import(&config_path, &manifest).unwrap();
        let imported = crate::load(&config_path).unwrap();
        assert_eq!(imported.node.network, custom.network);
        assert_eq!(imported.node.port, 58445);
        assert_eq!(imported.node.network_definition, custom.network_definition);
        assert_eq!(imported.connections.profiles.len(), 2);
        let custom_id = imported.connections.active_id.clone().unwrap();
        let custom_node = imported.node.clone();
        fs::remove_dir_all(&source).unwrap();
        let imported = crate::load(&config_path).unwrap();
        assert!(imported.node.certificate.as_ref().unwrap().is_file());
        assert_eq!(
            crate::network::resolve(&imported.node)
                .unwrap()
                .constants
                .difficulty_starting,
            250
        );
        let mut switched = imported;
        switched.select_connection(&mainnet_id).unwrap();
        crate::save(&config_path, &switched).unwrap();
        let mut switched = crate::load(&config_path).unwrap();
        assert_eq!(switched.node, mainnet_node);
        switched.select_connection(&custom_id).unwrap();
        crate::save(&config_path, &switched).unwrap();
        assert_eq!(crate::load(&config_path).unwrap().node, custom_node);
        fs::remove_dir_all(directory).unwrap();
    }
}
