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
/// A `[node]` table is no longer a second owner once profiles are configured.
pub const STRAY_NODE: &str = "[node] is not used when [[connections.profiles]] are configured; edit the active profile instead";

/// An entry addressable by connection-profile ID.
pub trait ProfileId {
    fn profile_id(&self) -> &str;
}
impl ProfileId for ConnectionProfile {
    fn profile_id(&self) -> &str {
        &self.id
    }
}

/// One selection rule shared by persisted profiles and fallback network intent:
/// a missing `active_id` is only valid when there are no profiles at all.
fn select_index<T: ProfileId>(
    active_id: Option<&str>,
    profiles: &[T],
) -> Result<Option<usize>, String> {
    match active_id {
        Some(active) => profiles
            .iter()
            .position(|profile| profile.profile_id() == active)
            .map(Some)
            .ok_or_else(|| "Selected connection profile does not exist".into()),
        None if profiles.is_empty() => Ok(None),
        None => Err("Connection profiles require one selected profile".into()),
    }
}
pub fn select_profile<'a, T: ProfileId>(
    active_id: Option<&str>,
    profiles: &'a [T],
) -> Result<Option<&'a T>, String> {
    Ok(select_index(active_id, profiles)?.map(|index| &profiles[index]))
}

/// Unboxed membership iterator for stored nodes: profiles own the nodes once
/// they exist, otherwise the single-node store is the only stored node.
enum StoredNodes<'a> {
    Single(std::iter::Once<&'a NodeConfig>),
    Profiles(std::slice::Iter<'a, ConnectionProfile>),
}
impl<'a> Iterator for StoredNodes<'a> {
    type Item = &'a NodeConfig;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(nodes) => nodes.next(),
            Self::Profiles(profiles) => profiles.next().map(|profile| &profile.node),
        }
    }
}

/// Mutable counterpart of [`StoredNodes`].
enum StoredNodesMut<'a> {
    Single(std::iter::Once<&'a mut NodeConfig>),
    Profiles(std::slice::IterMut<'a, ConnectionProfile>),
}
impl<'a> Iterator for StoredNodesMut<'a> {
    type Item = &'a mut NodeConfig;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(nodes) => nodes.next(),
            Self::Profiles(profiles) => profiles.next().map(|profile| &mut profile.node),
        }
    }
}

impl ConnectionProfiles {
    /// Structural validation. The runtime node is the active profile itself, so
    /// there is no second copy to compare against.
    pub fn validate(&self) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        for profile in &self.profiles {
            if profile.id.is_empty() || profile.name.trim().is_empty() || !ids.insert(&profile.id) {
                return Err(
                    "Connection profiles require unique nonempty IDs and nonempty names".into(),
                );
            }
            profile.node.validate()?;
        }
        self.selected()?;
        Ok(())
    }
    /// The selected profile, or `Ok(None)` for a single-node configuration.
    pub fn selected(&self) -> Result<Option<&ConnectionProfile>, String> {
        select_profile(self.active_id.as_deref(), &self.profiles)
    }
    pub fn active(&self) -> Option<&ConnectionProfile> {
        self.selected().ok().flatten()
    }
    fn selected_index(&self) -> Result<Option<usize>, String> {
        select_index(self.active_id.as_deref(), &self.profiles)
    }
    /// `[connections]` is the persisted source of truth. A `[node]` table that
    /// equals the active profile is accepted as legacy input; a diverging one is
    /// rejected instead of silently overriding or dropping operator edits. With
    /// no profiles, `[node]` is the runtime node.
    pub fn resolve_node(&self, node: Option<NodeConfig>) -> Result<Option<NodeConfig>, String> {
        match self.selected()? {
            Some(profile) => {
                if let Some(node) = node
                    && node != profile.node
                {
                    return Err(STRAY_NODE.into());
                }
                Ok(None)
            }
            None => Ok(Some(node.unwrap_or_default())),
        }
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
    /// A single-node configuration with no profiles yet.
    pub fn from_node(node: NodeConfig) -> Self {
        Self {
            single_node: node,
            ..Self::default()
        }
    }
    /// The runtime node: the active profile when profiles exist, otherwise the
    /// private single-node store. Once profiles exist `single_node` has no
    /// meaning; an invalid selection is a programming error here because load,
    /// save and `Config::validate` are the canonical error points.
    pub fn node(&self) -> &NodeConfig {
        match self.connections.selected_index() {
            Ok(Some(index)) => &self.connections.profiles[index].node,
            Ok(None) => &self.single_node,
            Err(error) => panic!("{error}"),
        }
    }
    /// Write-through accessor. Editing the runtime node edits the active profile
    /// in place; there is no staged copy and no commit step. As with [`Self::node`],
    /// once profiles exist `single_node` has no meaning and an invalid selection
    /// panics rather than mutating the hidden store.
    pub fn node_mut(&mut self) -> &mut NodeConfig {
        match self.connections.selected_index() {
            Ok(Some(index)) => &mut self.connections.profiles[index].node,
            Ok(None) => &mut self.single_node,
            Err(error) => panic!("{error}"),
        }
    }
    /// Every stored node exactly once: the single-node store only when there are
    /// no profiles, otherwise each profile node. Never the runtime projection
    /// plus its source.
    pub fn stored_nodes(&self) -> impl Iterator<Item = &NodeConfig> {
        let profiles = &self.connections.profiles;
        let single = &self.single_node;
        if profiles.is_empty() {
            StoredNodes::Single(std::iter::once(single))
        } else {
            StoredNodes::Profiles(profiles.iter())
        }
    }
    /// Mutable counterpart of [`Self::stored_nodes`].
    pub fn stored_nodes_mut(&mut self) -> impl Iterator<Item = &mut NodeConfig> {
        let profiles = &mut self.connections.profiles;
        let single = &mut self.single_node;
        if profiles.is_empty() {
            StoredNodesMut::Single(std::iter::once(single))
        } else {
            StoredNodesMut::Profiles(profiles.iter_mut())
        }
    }
    /// Persist a credentialed single-node runtime as its first profile. The
    /// private store is left default; the stale copy does not need to survive.
    fn migrate_single_node(&mut self) -> Result<(), String> {
        if self.connections.profiles.is_empty() && self.single_node.certificate.is_some() {
            self.single_node.validate()?;
            let node = std::mem::take(&mut self.single_node);
            self.connections.active_id = Some(self.connections.insert(node));
        }
        Ok(())
    }
    /// Validate the selection and migrate a single-node configuration. Profiles
    /// own their node, so saving never folds it into a different profile.
    pub fn remember_connection(&mut self) -> Result<(), String> {
        if self.connections.profiles.is_empty() {
            return self.migrate_single_node();
        }
        self.connections.selected()?;
        Ok(())
    }
    pub fn add_connection(&mut self, node: NodeConfig) -> Result<(), String> {
        node.validate()?;
        self.migrate_single_node()?;
        self.connections.active_id = Some(self.connections.insert(node));
        Ok(())
    }
    pub fn select_connection(&mut self, id: &str) -> Result<(), String> {
        if !self
            .connections
            .profiles
            .iter()
            .any(|profile| profile.id == id)
        {
            return Err("Connection profile does not exist".into());
        }
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
        legacy_text_with(config, config.node())
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
        let config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("[node]"));
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node(), config.node());
        assert!(restored.connections.profiles.is_empty());
        assert_eq!(restored.connections.active_id, None);
        restored.validate().unwrap();
    }

    #[test]
    fn profiles_are_the_persisted_truth_and_node_is_derived() {
        let mut config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        let mut other = complete("testnet.fixture", Network::Testnet11);
        other.port = 58444;
        other.rpc = None;
        config.add_connection(other.clone()).unwrap();
        let selected = config.connections.active_id.clone().unwrap();
        let first = config.connections.profiles[0].id.clone();
        assert_eq!(config.node(), &other);

        let text = toml::to_string(&config).unwrap();
        assert!(!text.contains("[node]"), "{text}");
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node(), &other);
        assert_eq!(restored.connections, config.connections);
        restored.validate().unwrap();

        config.select_connection(&first).unwrap();
        assert_eq!(config.node().host, "mainnet.fixture");
        config.select_connection(&selected).unwrap();
        assert_eq!(config.node(), &other);
        let before = toml::to_string(&config).unwrap();
        assert!(config.select_connection("missing").is_err());
        assert_eq!(toml::to_string(&config).unwrap(), before);
    }

    #[test]
    fn matching_legacy_node_table_migrates_and_is_omitted_on_save() {
        let mut config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        config
            .add_connection(complete("testnet.fixture", Network::Testnet11))
            .unwrap();
        let text = legacy_text(&config);
        assert!(text.contains("[node]") && text.contains("[connections]"));
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node(), config.node());
        assert_eq!(restored.connections.active().unwrap().node, *config.node());
        restored.validate().unwrap();
        let saved = toml::to_string(&restored).unwrap();
        assert!(!saved.contains("[node]"));
        assert_eq!(
            toml::from_str::<Config>(&saved).unwrap().node(),
            restored.node()
        );
    }

    #[test]
    fn diverging_stray_node_tables_are_rejected_loudly() {
        let mut config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        config
            .add_connection(complete("testnet.fixture", Network::Testnet11))
            .unwrap();
        let mut stray = config.node().clone();
        stray.host = "edited.fixture".into();
        let error = toml::from_str::<Config>(&legacy_text_with(&config, &stray))
            .unwrap_err()
            .to_string();
        assert!(error.contains("edit the active profile"), "{error}");
        // `NodeConfig` fills omitted fields with defaults, so a partial table
        // would otherwise wipe credentials and reset the network to mainnet.
        let partial = NodeConfig {
            host: "partial.fixture".into(),
            ..Default::default()
        };
        let error = toml::from_str::<Config>(&legacy_text_with(&config, &partial))
            .unwrap_err()
            .to_string();
        assert!(error.contains("edit the active profile"), "{error}");
    }

    #[test]
    fn runtime_node_writes_through_to_the_active_profile() {
        let mut config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        config
            .add_connection(complete("testnet.fixture", Network::Testnet11))
            .unwrap();
        config.node_mut().host = "edited.fixture".into();
        assert_eq!(
            config.connections.active().unwrap().node.host,
            "edited.fixture"
        );
        config.validate().unwrap();
        config.remember_connection().unwrap();
        assert_eq!(config.node().host, "edited.fixture");
    }

    #[test]
    fn runtime_node_edits_round_trip_through_toml_without_a_node_table() {
        let mut config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        config
            .add_connection(complete("testnet.fixture", Network::Testnet11))
            .unwrap();
        config.node_mut().host = "edited.fixture".into();
        let text = toml::to_string(&config).unwrap();
        assert!(!text.contains("[node]"), "{text}");
        let restored: Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node().host, "edited.fixture");
        assert_eq!(
            restored.connections.active().unwrap().node.host,
            "edited.fixture"
        );
        restored.validate().unwrap();
        let again = toml::to_string(&restored).unwrap();
        assert_eq!(again, text);
        assert_eq!(
            toml::from_str::<Config>(&again).unwrap().node().host,
            "edited.fixture"
        );
    }

    #[test]
    #[should_panic(expected = "Selected connection profile does not exist")]
    fn runtime_node_accessor_rejects_an_invalid_selection() {
        let mut config = Config::from_node(complete("a.fixture", Network::Mainnet));
        config
            .add_connection(complete("b.fixture", Network::Testnet11))
            .unwrap();
        config.connections.active_id = Some("missing".into());
        let _ = config.node();
    }

    #[test]
    #[should_panic(expected = "Selected connection profile does not exist")]
    fn mutable_runtime_node_accessor_rejects_an_invalid_selection() {
        let mut config = Config::from_node(complete("a.fixture", Network::Mainnet));
        config
            .add_connection(complete("b.fixture", Network::Testnet11))
            .unwrap();
        config.connections.active_id = Some("missing".into());
        let _ = config.node_mut();
    }

    #[test]
    fn profile_selection_requires_one_existing_active_profile() {
        let mut config = Config::default();
        config.connections.profiles = vec![ConnectionProfile {
            id: "a".into(),
            name: "A".into(),
            node: NodeConfig::default(),
        }];
        assert_eq!(
            config.validate().unwrap_err(),
            "Connection profiles require one selected profile"
        );
        config.connections.active_id = Some("missing".into());
        assert_eq!(
            config.validate().unwrap_err(),
            "Selected connection profile does not exist"
        );
        config.connections.active_id = Some("a".into());
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
        let mut config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        let original = config.node().clone();
        let mut other = complete("testnet.fixture", Network::Testnet11);
        other.port = 58444;
        other.rpc = None;
        config.add_connection(other.clone()).unwrap();
        let selected = config.connections.active_id.clone().unwrap();
        let first = config.connections.profiles[0].id.clone();
        assert_eq!(config.connections.profiles.len(), 2);
        assert_eq!(config.node(), &other);
        config.select_connection(&first).unwrap();
        assert_eq!(config.node(), &original);
        config.node_mut().host = "edited-mainnet.fixture".into();
        config.select_connection(&selected).unwrap();
        assert_eq!(config.node(), &other);
        config.select_connection(&first).unwrap();
        assert_eq!(config.node().host, "edited-mainnet.fixture");
        config.validate().unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.node(), config.node());
        assert_eq!(restored.connections.profiles.len(), 2);
    }

    #[test]
    fn repeated_imports_create_distinct_profiles_and_names() {
        let mut config = Config::from_node(complete("mainnet.fixture", Network::Mainnet));
        let node = config.node().clone();
        config.add_connection(node.clone()).unwrap();
        config.add_connection(node).unwrap();
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

    #[test]
    fn adding_a_connection_preserves_an_edited_active_profile() {
        let mut config = Config::from_node(complete("a.fixture", Network::Mainnet));
        config
            .add_connection(complete("b.fixture", Network::Testnet11))
            .unwrap();
        let a = config.connections.profiles[0].id.clone();
        config.select_connection(&a).unwrap();
        config.node_mut().host = "edited-a.fixture".into();

        config
            .add_connection(complete("c.fixture", Network::Testnet11))
            .unwrap();
        config.select_connection(&a).unwrap();

        assert_eq!(config.node().host, "edited-a.fixture");
        let b = config
            .connections
            .profiles
            .iter()
            .find(|profile| profile.node.host == "b.fixture")
            .unwrap();
        let c = config
            .connections
            .profiles
            .iter()
            .find(|profile| profile.node.host == "c.fixture")
            .unwrap();
        assert_eq!(b.node.host, "b.fixture");
        assert_eq!(c.node.host, "c.fixture");
        assert_ne!(c.node.host, "edited-a.fixture");
        config.validate().unwrap();
    }
}
