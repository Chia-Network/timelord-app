use crate::{Config, Network, NodeConfig};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionProfiles {
    pub active_id: Option<String>,
    pub profiles: Vec<ConnectionProfile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub node: NodeConfig,
}
impl ConnectionProfiles {
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
        match self
            .active_id
            .as_ref()
            .and_then(|id| self.profiles.iter().find(|p| p.id == *id))
        {
            Some(profile) if profile.node == *active_node => {}
            None if self.active_id.is_none() && self.profiles.is_empty() => {}
            _ => return Err("Selected connection profile must match the active node".into()),
        }
        Ok(())
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
    /// Keep `[node]` as the runtime/backward-compatible projection. Profile
    /// snapshots are synchronized at the persistence and selection boundaries.
    pub fn remember_connection(&mut self) -> Result<(), String> {
        if let Some(id) = &self.connections.active_id {
            let profile = self
                .connections
                .profiles
                .iter_mut()
                .find(|p| p.id == *id)
                .ok_or("Selected connection profile does not exist")?;
            profile.node = self.node.clone();
        } else if !self.connections.profiles.is_empty() {
            return Err("Select an existing connection profile".into());
        } else if self.node.certificate.is_some() {
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
        if self.network.label().parse::<Network>()? != self.network {
            return Err("Invalid network selection".into());
        }
        match (&self.network, &self.network_definition) {
            (Network::Custom(_), Some(definition)) => definition.validate(&self.network)?,
            (Network::Custom(_), None) => {
                return Err("Import a definition for this custom network".into());
            }
            (_, Some(_)) => return Err("Built-in networks cannot use a custom definition".into()),
            _ => {}
        }
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
    fn legacy() -> Config {
        let mut config = Config::default();
        config.node.host = "mainnet.fixture".into();
        config.node.certificate = Some("mainnet.crt".into());
        config.node.private_key = Some("mainnet.key".into());
        config.node.ca_certificate = Some("mainnet-ca.crt".into());
        config.node.rpc = Some(crate::NodeRpcConfig {
            host: "rpc-mainnet.fixture".into(),
            port: 8555,
            server_name: "chia.net".into(),
            certificate: "rpc-mainnet.crt".into(),
            private_key: "rpc-mainnet.key".into(),
            ca_certificate: "rpc-mainnet-ca.crt".into(),
        });
        config
    }
    #[test]
    fn first_import_preserves_legacy_and_switches_complete_connections() {
        let mut config = legacy();
        let original = config.node.clone();
        let mut other = original.clone();
        other.host = "testnet.fixture".into();
        other.port = 58444;
        other.network = Network::Testnet11;
        other.private_key = Some("testnet.key".into());
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
        let before = toml::to_string(&config).unwrap();
        assert!(config.select_connection("missing").is_err());
        assert_eq!(toml::to_string(&config).unwrap(), before);
    }
    #[test]
    fn repeated_imports_create_distinct_profiles_and_names() {
        let mut config = legacy();
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
