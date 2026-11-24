use crate::{Config, Network, NodeConfig};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionProfiles {
    pub active_id: Option<String>,
    pub profiles: Vec<ConnectionProfile>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub node: NodeConfig,
}
impl ConnectionProfiles {
    /// Structural validation plus the runtime projection invariant. The runtime
    /// node is derived from the active profile, so a legacy `[node]` table never
    /// fails here and can never clobber a profile.
    pub fn validate(&self, active_node: &NodeConfig) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        for profile in &self.profiles {
            if profile.id.is_empty() || profile.name.trim().is_empty() || !ids.insert(&profile.id) {
                return Err(
                    "Connection profiles require unique nonempty IDs and nonempty names".into(),
                );
            }
            profile.node.validate()?;
        }
        match self.active() {
            None if self.active_id.is_none() && self.profiles.is_empty() => Ok(()),
            Some(profile) if profile.node == *active_node => Ok(()),
            Some(_) => Err("The active connection profile must match [node]".into()),
            None => Err("Connection profiles require one existing active profile".into()),
        }
    }
    pub fn active(&self) -> Option<&ConnectionProfile> {
        let id = self.active_id.as_deref()?;
        self.profiles.iter().find(|p| p.id == id)
    }
    fn active_mut(&mut self) -> Option<&mut ConnectionProfile> {
        let id = self.active_id.as_deref()?;
        self.profiles.iter_mut().find(|p| p.id == id)
    }
    /// `[connections]` is the persisted source of truth. A `[node]` table is
    /// accepted for compatibility but ignored whenever profiles are used, so a
    /// partial legacy table cannot wipe credentials or reset the chain. Without
    /// profiles, `[node]` is the runtime node.
    pub fn resolve_node(&self, node: Option<NodeConfig>) -> NodeConfig {
        self.active()
            .map(|profile| profile.node.clone())
            .or(node)
            .unwrap_or_default()
    }
    fn insert(&mut self, node: NodeConfig) -> String {
        let id = (1..)
            .map(|i| format!("connection-{i}"))
            .find(|id| self.profiles.iter().all(|p| p.id != *id))
            .expect("available profile ID");
        let network = match node.network {
            Network::Mainnet => "Mainnet",
            Network::Testnet11 => "Testnet11",
            _ => node.network.label(),
        };
        let base = format!("{network} · {}:{}", node.host, node.port);
        let mut name = base.clone();
        let mut suffix = 2;
        while self.profiles.iter().any(|p| p.name == name) {
            name = format!("{base} ({suffix})");
            suffix += 1;
        }
        self.profiles.push(ConnectionProfile {
            id: id.clone(),
            name,
            node,
        });
        id
    }
}
impl Config {
    /// Fold the runtime node into its profile. Persistence then writes only
    /// `[connections]`; a single-node configuration becomes a profile here.
    pub fn remember_connection(&mut self) -> Result<(), String> {
        if let Some(profile) = self.connections.active_mut() {
            profile.node = self.node.clone();
            return Ok(());
        }
        if !self.connections.profiles.is_empty() {
            return Err("Select an existing connection profile".into());
        }
        if self.node.certificate.is_some() {
            self.node.validate()?;
            self.connections.active_id = Some(self.connections.insert(self.node.clone()));
        }
        Ok(())
    }
    pub fn add_connection(&mut self, node: NodeConfig) -> Result<(), String> {
        node.validate()?;
        self.remember_connection()?;
        self.connections.active_id = Some(self.connections.insert(node.clone()));
        self.node = node;
        Ok(())
    }
    pub fn select_connection(&mut self, id: &str) -> Result<(), String> {
        if self.connections.active_id.as_deref() == Some(id) {
            return Ok(());
        }
        let node = self
            .connections
            .profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or("Connection profile does not exist")?
            .node
            .clone();
        self.remember_connection()?;
        self.node = node;
        self.connections.active_id = Some(id.into());
        Ok(())
    }
}

impl NodeConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.chain_identity()?;
        if self.host.trim().is_empty() || self.port == 0 {
            return Err("Node host and nonzero port are required".into());
        }
        if let Some(rpc) = &self.rpc
            && (rpc.host.trim().is_empty()
                || rpc.port == 0
                || rpc.server_name.trim().is_empty()
                || [&rpc.certificate, &rpc.private_key, &rpc.ca_certificate]
                    .iter()
                    .any(|p| p.as_os_str().is_empty()))
        {
            return Err(
                "Node RPC requires a host, port, TLS server name and all three credential paths"
                    .into(),
            );
        }
        let credentials = [&self.certificate, &self.private_key, &self.ca_certificate];
        let supplied = credentials.iter().filter(|p| p.is_some()).count();
        if supplied != 0 && supplied != 3 {
            return Err(
                "Supply all three node credential paths, or omit all three during setup".into(),
            );
        }
        if credentials
            .iter()
            .any(|p| p.as_ref().is_some_and(|p| p.as_os_str().is_empty()))
        {
            return Err("Node credential paths cannot be empty".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn complete(host: &str, network: Network) -> NodeConfig {
        NodeConfig {
            host: host.into(),
            network,
            certificate: Some(format!("{host}.crt").into()),
            private_key: Some(format!("{host}.key").into()),
            ca_certificate: Some(format!("{host}-ca.crt").into()),
            rpc: Some(crate::NodeRpcConfig {
                host: format!("rpc-{host}"),
                port: 8555,
                server_name: "chia.net".into(),
                certificate: format!("rpc-{host}.crt").into(),
                private_key: format!("rpc-{host}.key").into(),
                ca_certificate: format!("rpc-{host}-ca.crt").into(),
            }),
            ..Default::default()
        }
    }
    /// Serializes the pre-profile layout so migration tests use real old files.
    #[derive(Serialize)]
    struct Legacy<'a> {
        schema_version: u32,
        node: &'a NodeConfig,
        connections: &'a ConnectionProfiles,
        service: &'a crate::ServiceConfig,
        hardware: &'a crate::HardwareConfig,
    }
    fn legacy_text(config: &Config) -> String {
        legacy_text_with(config, &config.node)
    }
    fn legacy_text_with(config: &Config, node: &NodeConfig) -> String {
        toml::to_string(&Legacy {
            schema_version: config.schema_version,
            node,
            connections: &config.connections,
            service: &config.service,
            hardware: &config.hardware,
        })
        .unwrap()
    }

    #[test]
    fn single_node_configuration_keeps_node_and_never_invents_profiles() {
        let config = Config {
            node: complete("mainnet.fixture", Network::Mainnet),
            ..Default::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("[node]"));
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node, config.node);
        assert!(restored.connections.profiles.is_empty());
        assert_eq!(restored.connections.active_id, None);
        restored.validate().unwrap();
    }

    #[test]
    fn profiles_are_the_persisted_truth_and_node_is_derived() {
        let mut config = Config {
            node: complete("mainnet.fixture", Network::Mainnet),
            ..Default::default()
        };
        let mut other = complete("testnet.fixture", Network::Testnet11);
        other.port = 58444;
        other.rpc = None;
        config.add_connection(other.clone()).unwrap();
        let selected = config.connections.active_id.clone().unwrap();
        let first = config.connections.profiles[0].id.clone();
        assert_eq!(config.node, other);

        let text = toml::to_string(&config).unwrap();
        assert!(!text.contains("[node]"), "{text}");
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node, other);
        assert_eq!(restored.connections, config.connections);
        restored.validate().unwrap();

        config.select_connection(&first).unwrap();
        assert_eq!(config.node.host, "mainnet.fixture");
        config.select_connection(&selected).unwrap();
        assert_eq!(config.node, other);
        let before = toml::to_string(&config).unwrap();
        assert!(config.select_connection("missing").is_err());
        assert_eq!(toml::to_string(&config).unwrap(), before);
    }

    #[test]
    fn profiles_win_over_a_legacy_node_table() {
        let mut config = Config {
            node: complete("mainnet.fixture", Network::Mainnet),
            ..Default::default()
        };
        config
            .add_connection(complete("testnet.fixture", Network::Testnet11))
            .unwrap();
        let expected = config.node.clone();
        config.node.host = "edited.fixture".into();
        let text = legacy_text(&config);
        assert!(text.contains("[node]") && text.contains("[connections]"));
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node, expected);
        assert_eq!(restored.connections.active().unwrap().node, expected);
        restored.validate().unwrap();
        let saved = toml::to_string(&restored).unwrap();
        assert!(!saved.contains("[node]"));
        assert_eq!(
            toml::from_str::<Config>(&saved).unwrap().node,
            restored.node
        );
    }

    #[test]
    fn a_partial_node_table_cannot_clobber_the_active_profile() {
        let mut config = Config {
            node: complete("mainnet.fixture", Network::Mainnet),
            ..Default::default()
        };
        config
            .add_connection(complete("testnet.fixture", Network::Testnet11))
            .unwrap();
        let expected = config.node.clone();
        // `NodeConfig` fills omitted fields with defaults, so a partial table
        // would otherwise wipe credentials and reset the network to mainnet.
        let partial = NodeConfig {
            host: "partial.fixture".into(),
            ..Default::default()
        };
        let text = legacy_text_with(&config, &partial);
        assert!(text.contains("partial.fixture"));
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node, expected);
        assert_eq!(restored.connections.active().unwrap().node, expected);
        restored.validate().unwrap();
    }

    #[test]
    fn diverging_projection_is_rejected_instead_of_silently_dropped() {
        let mut config = Config {
            node: complete("mainnet.fixture", Network::Mainnet),
            ..Default::default()
        };
        config
            .add_connection(complete("testnet.fixture", Network::Testnet11))
            .unwrap();
        config.node.host = "unsaved.fixture".into();
        assert!(config.validate().is_err());
        config.remember_connection().unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn profile_selection_requires_one_existing_active_profile() {
        let mut config = Config::default();
        config.connections.profiles = vec![ConnectionProfile {
            id: "a".into(),
            name: "A".into(),
            node: NodeConfig::default(),
        }];
        assert!(config.validate().is_err());
        config.connections.active_id = Some("missing".into());
        assert!(config.validate().is_err());
        config.connections.active_id = Some("a".into());
        config.node = NodeConfig::default();
        config.validate().unwrap();

        let mut empty = Config::default();
        empty.connections.active_id = Some("a".into());
        assert!(empty.validate().is_err());

        let mut duplicate = Config::default();
        duplicate.connections.profiles = vec![
            ConnectionProfile {
                id: "a".into(),
                name: "A".into(),
                node: NodeConfig::default(),
            },
            ConnectionProfile {
                id: "a".into(),
                name: "B".into(),
                node: NodeConfig::default(),
            },
        ];
        duplicate.connections.active_id = Some("a".into());
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn first_import_preserves_legacy_and_switches_complete_connections() {
        let mut config = Config {
            node: complete("mainnet.fixture", Network::Mainnet),
            ..Default::default()
        };
        let original = config.node.clone();
        let mut other = complete("testnet.fixture", Network::Testnet11);
        other.port = 58444;
        other.rpc = None;
        config.add_connection(other.clone()).unwrap();
        let selected = config.connections.active_id.clone().unwrap();
        let first = config.connections.profiles[0].id.clone();
        assert_eq!(config.connections.profiles.len(), 2);
        assert_eq!(config.node, other);
        config.select_connection(&first).unwrap();
        assert_eq!(config.node, original);
        config.node.host = "edited-mainnet.fixture".into();
        config.select_connection(&selected).unwrap();
        assert_eq!(config.node, other);
        config.select_connection(&first).unwrap();
        assert_eq!(config.node.host, "edited-mainnet.fixture");
        config.validate().unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.node, config.node);
        assert_eq!(restored.connections.profiles.len(), 2);
    }

    #[test]
    fn repeated_imports_create_distinct_profiles_and_names() {
        let mut config = Config {
            node: complete("mainnet.fixture", Network::Mainnet),
            ..Default::default()
        };
        config.add_connection(config.node.clone()).unwrap();
        config.add_connection(config.node.clone()).unwrap();
        assert_eq!(config.connections.profiles.len(), 3);
        let names: std::collections::HashSet<_> = config
            .connections
            .profiles
            .iter()
            .map(|p| &p.name)
            .collect();
        assert_eq!(names.len(), 3);
        config.validate().unwrap();
    }
}
