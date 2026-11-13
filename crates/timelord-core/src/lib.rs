//! Shared configuration and topology. This crate performs no I/O or hardware work.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub node: NodeConfig,
    pub service: ServiceConfig,
    pub hardware: HardwareConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            node: NodeConfig::default(),
            service: ServiceConfig::default(),
            hardware: HardwareConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeConfig {
    pub host: String,
    pub port: u16,
    pub network: Network,
    pub network_definition: Option<NetworkDefinition>,
    pub server_name: Option<String>,
    pub certificate: Option<PathBuf>,
    pub private_key: Option<PathBuf>,
    pub ca_certificate: Option<PathBuf>,
    pub rpc: Option<NodeRpcConfig>,
}

/// Separate private-service identity for read-only full-node status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRpcConfig {
    pub host: String,
    pub port: u16,
    pub server_name: String,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub ca_certificate: PathBuf,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 8444,
            network: Network::Mainnet,
            network_definition: None,
            server_name: None,
            certificate: None,
            private_key: None,
            ca_certificate: None,
            rpc: None,
        }
    }
}

mod network;
pub use network::{ChainIdentity, Network, NetworkDefinition, NetworkValue};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceConfig {
    pub status_listen: SocketAddr,
    pub observability_listen: Option<SocketAddr>,
    /// Explicitly allow unauthenticated deployment probes outside loopback.
    pub allow_remote_observability: bool,
    pub supervisor_observability_listen: SocketAddr,
    pub control_socket: Option<PathBuf>,
    pub tuning_paused: bool,
    pub start_automatically: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            status_listen: SocketAddr::from(([127, 0, 0, 1], 9257)),
            observability_listen: None,
            allow_remote_observability: false,
            supervisor_observability_listen: SocketAddr::from(([127, 0, 0, 1], 9259)),
            control_socket: None,
            tuning_paused: false,
            start_automatically: true,
        }
    }
}

impl ServiceConfig {
    /// Ports owned by service endpoints; board client ports must avoid them.
    pub fn reserved_ports(&self) -> HashSet<u16> {
        let mut ports = HashSet::from([
            self.status_listen.port(),
            self.supervisor_observability_listen.port(),
        ]);
        ports.extend(self.observability_listen.map(|a| a.port()));
        ports
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HardwareConfig {
    pub client_binary: Option<PathBuf>,
    pub driver_library: Option<PathBuf>,
    pub boards: Vec<BoardConfig>,
    pub remembered_boards: Vec<BoardConfig>,
    pub automatic_state: Option<PathBuf>,
    /// Omit for automatic ranking. Explicit assignments require three boards.
    pub chain_assignments: Option<ChainAssignments>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainAssignments {
    pub challenge: String,
    pub reward: String,
    pub infused_challenge: String,
}
impl ChainAssignments {
    pub fn serials(&self) -> [&str; 3] {
        [&self.challenge, &self.reward, &self.infused_challenge]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardConfig {
    pub id: String,
    pub name: String,
    pub client_port: u16,
    pub device_location: Option<u32>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub profile: ProfileMode,
    #[serde(default)]
    pub automatic: AutomaticConfig,
    #[serde(default = "default_frequency")]
    pub frequency_mhz: f64,
    #[serde(default = "default_voltage")]
    pub voltage_v: f64,
    #[serde(default = "default_temperature_limit")]
    pub temperature_limit_c: f64,
}
fn default_frequency() -> f64 {
    1450.0
}
fn default_voltage() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMode {
    #[default]
    Automatic,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomaticConfig {
    pub start_frequency_mhz: f64,
    pub start_voltage_v: f64,
    pub min_frequency_mhz: f64,
    pub max_frequency_mhz: f64,
    pub min_voltage_v: f64,
    pub healthy_seconds: u64,
    pub cooldown_seconds: u64,
    /// Healthy-work interval before retesting a rejected clock; zero disables retries.
    pub retest_after_seconds: u64,
    /// Read existing configs without reviving the removed voltage search.
    #[serde(skip_serializing)]
    pub voltage_after_seconds: u64,
    pub recovery_window_seconds: u64,
    pub recovery_loss_percent: f64,
    pub recovery_bad_windows: u32,
    pub minimum_proofs_per_chain: usize,
}
impl Default for AutomaticConfig {
    fn default() -> Self {
        Self {
            start_frequency_mhz: 1450.,
            start_voltage_v: 1.,
            min_frequency_mhz: 1000.,
            max_frequency_mhz: 2200.,
            min_voltage_v: 0.9,
            healthy_seconds: 120,
            cooldown_seconds: 900,
            retest_after_seconds: 7200,
            voltage_after_seconds: 3600,
            recovery_window_seconds: 300,
            recovery_loss_percent: 5.,
            recovery_bad_windows: 2,
            minimum_proofs_per_chain: 3,
        }
    }
}
impl AutomaticConfig {
    pub fn contains(&self, frequency: f64, voltage: f64) -> bool {
        frequency.is_finite()
            && voltage.is_finite()
            && (self.min_frequency_mhz..=self.max_frequency_mhz).contains(&frequency)
            && (self.min_voltage_v..=1.).contains(&voltage)
    }
    fn validate(&self) -> bool {
        (200. ..=2200.).contains(&self.min_frequency_mhz)
            && (self.min_frequency_mhz..=2200.).contains(&self.max_frequency_mhz)
            && (0.7..=1.).contains(&self.min_voltage_v)
            && self.contains(self.start_frequency_mhz, self.start_voltage_v)
            && self.healthy_seconds >= 30
            && self.cooldown_seconds >= self.healthy_seconds
            && (self.retest_after_seconds == 0
                || (self.cooldown_seconds..=86400).contains(&self.retest_after_seconds))
            && self.recovery_window_seconds >= self.healthy_seconds
            && self.recovery_window_seconds <= 3600
            && self.recovery_loss_percent.is_finite()
            && (0.1..=50.).contains(&self.recovery_loss_percent)
            && (2..=12).contains(&self.recovery_bad_windows)
            && self.minimum_proofs_per_chain >= 3
    }
}

impl BoardConfig {
    pub fn operating_point(&self) -> (f64, f64) {
        match self.profile {
            ProfileMode::Automatic => (
                self.automatic.start_frequency_mhz,
                self.automatic.start_voltage_v,
            ),
            ProfileMode::Manual => (self.frequency_mhz, self.voltage_v),
        }
    }
    pub fn profile_name(&self) -> &'static str {
        match self.profile {
            ProfileMode::Automatic => "Automatic",
            ProfileMode::Manual => "Manual",
        }
    }
}
fn default_temperature_limit() -> f64 {
    55.0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Chain {
    Challenge,
    Reward,
    InfusedChallenge,
}

impl Chain {
    pub const ALL: [Self; 3] = [Self::Challenge, Self::Reward, Self::InfusedChallenge];
}

/// Physical engines selected by the native client for each board workload.
pub fn engine_mask(engines: usize) -> Option<u8> {
    match engines {
        1 => Some(0b010),
        2 => Some(0b011),
        3 => Some(0b111),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Assignment {
    pub board_id: String,
    pub engine: u8,
    pub chain: Chain,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("Unsupported schema_version; expected 1".into());
        }
        if self.node.network.label().parse::<Network>()? != self.node.network {
            return Err("Invalid network selection".into());
        }
        match (&self.node.network, &self.node.network_definition) {
            (Network::Custom(_), Some(definition)) => definition.validate(&self.node.network)?,
            (Network::Custom(_), None) => {
                return Err("Import a definition for this custom network".into());
            }
            (_, Some(_)) => return Err("Built-in networks cannot use a custom definition".into()),
            _ => {}
        }
        if self.node.host.trim().is_empty() || self.node.port == 0 {
            return Err("Node host and nonzero port are required".into());
        }
        if let Some(rpc) = &self.node.rpc
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
        let credentials = [
            &self.node.certificate,
            &self.node.private_key,
            &self.node.ca_certificate,
        ];
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
        if !self.service.status_listen.ip().is_loopback() || self.service.status_listen.port() == 0
        {
            return Err("The status endpoint must use a loopback address and nonzero port".into());
        }
        if self.hardware.boards.len() > 3 {
            return Err("This version supports up to three configured boards".into());
        }
        if self
            .service
            .observability_listen
            .is_some_and(|a| a.port() == 0 || a.port() == self.service.status_listen.port())
        {
            return Err(
                "Observability requires a nonzero port distinct from status and client ports"
                    .into(),
            );
        }
        if !self.service.allow_remote_observability
            && (self
                .service
                .observability_listen
                .is_some_and(|a| !a.ip().is_loopback())
                || !self
                    .service
                    .supervisor_observability_listen
                    .ip()
                    .is_loopback())
        {
            return Err("Remote observability requires explicit allow_remote_observability = true (no built-in authentication or TLS)".into());
        }
        let supervisor = self.service.supervisor_observability_listen;
        if supervisor.port() == 0
            || supervisor.port() == self.service.status_listen.port()
            || self
                .service
                .observability_listen
                .is_some_and(|a| a.port() == supervisor.port())
        {
            return Err(
                "Supervisor observability must use a nonzero port distinct from worker endpoints"
                    .into(),
            );
        }
        let mut ids = HashSet::new();
        let mut ports = self.service.reserved_ports();
        let mut locations = HashSet::new();
        let mut serials = HashSet::new();
        for b in &self.hardware.boards {
            if let Some(serial) = &b.serial
                && (serial.trim().is_empty() || !serials.insert(serial.as_str()))
            {
                return Err("Board serials must be nonempty and unique".into());
            }
            if !b.automatic.validate() {
                return Err("Automatic settings require a 200–2200 MHz range, requested 0.7–1.0 V, and bounded observation/cooldown windows".into());
            }
            if !b.frequency_mhz.is_finite()
                || !(200.0..=2200.0).contains(&b.frequency_mhz)
                || !b.voltage_v.is_finite()
                || !(0.7..=1.0).contains(&b.voltage_v)
                || !b.temperature_limit_c.is_finite()
                || !(20.0..=55.0).contains(&b.temperature_limit_c)
            {
                return Err("Board settings require 200–2200 MHz, 0.7–1.0 V and a temperature limit no higher than 55°C".into());
            }
            if self.hardware.client_binary.is_some() && b.device_location.is_none() {
                return Err(
                    "Managed clients require an explicit USB location for every board".into(),
                );
            }
            if b.id.trim().is_empty() || b.name.trim().is_empty() {
                return Err("Each board needs a nonempty id and name".into());
            }
            if !ids.insert(&b.id) {
                return Err(format!("Duplicate board id: {}", b.id));
            }
            if b.client_port == 0 || !ports.insert(b.client_port) {
                return Err(format!(
                    "Board {} needs a unique, nonzero client port",
                    b.id
                ));
            }
            if let Some(location) = b.device_location
                && (location == 0 || !locations.insert(location))
            {
                return Err("Explicit device locations must be nonzero and unique; location zero is ambiguous".into());
            }
        }
        if let Some(mapping) = &self.hardware.chain_assignments {
            let selected = mapping.serials();
            if self.hardware.boards.len() != 3
                || selected.iter().copied().collect::<HashSet<_>>().len() != 3
                || selected.iter().any(|s| !serials.contains(s))
            {
                return Err(
                    "Explicit chain assignments require three distinct configured board serials"
                        .into(),
                );
            }
        }
        Ok(())
    }

    /// A configured plan, not a claim about detected hardware or connected clients.
    pub fn assignments(&self) -> Result<Vec<Assignment>, String> {
        self.validate()?;
        let mut boards: Vec<_> = self.hardware.boards.iter().collect();
        if let Some(mapping) = &self.hardware.chain_assignments {
            boards = mapping
                .serials()
                .iter()
                .map(|serial| {
                    boards
                        .iter()
                        .copied()
                        .find(|b| b.serial.as_deref() == Some(*serial))
                        .expect("validated serial")
                })
                .collect();
        }
        if boards.is_empty() {
            return Ok(vec![]);
        }
        let counts = match boards.len() {
            1 => [3, 0, 0],
            2 => [2, 1, 0],
            _ => [1, 1, 1],
        };
        let mut engine = counts.map(|count| if count == 1 { 1 } else { 0 });
        Ok(Chain::ALL
            .into_iter()
            .enumerate()
            .map(|(i, chain)| {
                let index = if i < counts[0] {
                    0
                } else if i < counts[0] + counts[1] {
                    1
                } else {
                    2
                };
                let assignment = Assignment {
                    board_id: boards[index].id.clone(),
                    engine: engine[index],
                    chain,
                };
                engine[index] += 1;
                assignment
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn board(id: &str, port: u16) -> BoardConfig {
        BoardConfig {
            id: id.into(),
            name: id.into(),
            client_port: port,
            device_location: None,
            serial: None,
            profile: ProfileMode::Automatic,
            automatic: AutomaticConfig::default(),
            frequency_mhz: default_frequency(),
            voltage_v: default_voltage(),
            temperature_limit_c: default_temperature_limit(),
        }
    }
    #[test]
    fn deployment_listener_is_optional_and_cannot_collide_with_local_endpoints() {
        let mut c = Config::default();
        assert!(c.service.observability_listen.is_none());
        c.service.observability_listen = Some("0.0.0.0:9258".parse().unwrap());
        assert!(c.validate().is_err());
        c.service.allow_remote_observability = true;
        assert!(c.validate().is_ok());
        c.service.observability_listen = Some("0.0.0.0:9257".parse().unwrap());
        assert!(c.validate().is_err());
        c.service.observability_listen = Some("127.0.0.1:0".parse().unwrap());
        assert!(c.validate().is_err());
        c.service.observability_listen = Some("[::]:18110".parse().unwrap());
        c.hardware.boards.push(board("a", 18110));
        assert!(c.validate().is_err());
        c.service.observability_listen = Some("[::]:9258".parse().unwrap());
        assert!(c.validate().is_ok());
        c.service.status_listen = "0.0.0.0:9257".parse().unwrap();
        assert!(c.validate().is_err());
    }
    #[test]
    fn remote_supervisor_probes_also_require_explicit_opt_in() {
        let mut config = Config::default();
        config.service.supervisor_observability_listen = "[::]:9259".parse().unwrap();
        assert!(config.validate().is_err());
        config.service.allow_remote_observability = true;
        assert!(config.validate().is_ok());
        let serialized = toml::to_string(&config).unwrap();
        let restored: Config = toml::from_str(&serialized).unwrap();
        assert!(restored.service.allow_remote_observability);
        assert!(restored.validate().is_ok());
    }

    #[test]
    fn old_voltage_search_key_is_read_only_and_recovery_limits_are_validated() {
        let value: AutomaticConfig = toml::from_str("voltage_after_seconds=3600").unwrap();
        assert!(value.validate());
        assert!(
            !toml::to_string(&value)
                .unwrap()
                .contains("voltage_after_seconds")
        );
        let mut invalid = value.clone();
        invalid.recovery_loss_percent = f64::NAN;
        assert!(!invalid.validate());
        invalid = value.clone();
        invalid.recovery_bad_windows = 1;
        assert!(!invalid.validate());
        invalid = value;
        invalid.recovery_window_seconds = 30;
        assert!(!invalid.validate());
    }

    #[test]
    fn periodic_retest_defaults_are_bounded_and_can_be_disabled() {
        let mut config = AutomaticConfig::default();
        assert_eq!(config.retest_after_seconds, 7200);
        config.retest_after_seconds = 0;
        assert!(config.validate());
        config.retest_after_seconds = 899;
        assert!(!config.validate());
        config.retest_after_seconds = 86400;
        assert!(config.validate());
        config.retest_after_seconds = 86401;
        assert!(!config.validate());
    }
    #[test]
    fn profiles_are_automatic_by_default_and_manual_is_explicit() {
        let mut b: BoardConfig = toml::from_str("id='a'\nname='A'\nclient_port=18110").unwrap();
        assert_eq!(b.profile, ProfileMode::Automatic);
        assert_eq!(b.operating_point(), (1450.0, 1.0));
        b.frequency_mhz = 1540.0;
        assert_eq!(b.operating_point(), (1450.0, 1.0));
        b.profile = ProfileMode::Manual;
        assert_eq!(b.operating_point(), (1540.0, 1.0));
        for removed in ["default", "calibrated"] {
            assert!(
                toml::from_str::<BoardConfig>(&format!(
                    "id='a'\nname='A'\nclient_port=18110\nprofile='{removed}'"
                ))
                .is_err()
            );
        }
        let mut c = Config::default();
        c.hardware.boards.push(b);
        c.hardware.boards[0].frequency_mhz = 2201.0;
        assert!(c.validate().is_err());
        c.hardware.boards[0].frequency_mhz = 1540.0;
        c.hardware.boards[0].automatic.start_voltage_v = 1.01;
        assert!(c.validate().is_err());
    }
    #[test]
    fn app_startup_defaults_on_and_preserves_explicit_opt_out() {
        let defaults: ServiceConfig = toml::from_str("status_listen='127.0.0.1:9257'").unwrap();
        assert!(defaults.start_automatically);
        let disabled: ServiceConfig = toml::from_str("start_automatically=false").unwrap();
        assert!(!disabled.start_automatically);
        let restored: ServiceConfig = toml::from_str(&toml::to_string(&disabled).unwrap()).unwrap();
        assert!(!restored.start_automatically);
    }
    #[test]
    fn explicit_assignments_follow_serials_and_reject_incomplete_or_ambiguous_maps() {
        let mut c = Config::default();
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            let mut b = board(id, 18110 + i as u16);
            b.serial = Some(format!("serial-{id}"));
            c.hardware.boards.push(b);
        }
        c.hardware.chain_assignments = Some(ChainAssignments {
            challenge: "serial-c".into(),
            reward: "serial-a".into(),
            infused_challenge: "serial-b".into(),
        });
        let original = c.assignments().unwrap();
        assert_eq!(
            original
                .iter()
                .map(|a| a.board_id.as_str())
                .collect::<Vec<_>>(),
            ["c", "a", "b"]
        );
        c.hardware.boards.reverse();
        for (i, b) in c.hardware.boards.iter_mut().enumerate() {
            b.device_location = Some(100 + i as u32);
        }
        assert_eq!(c.assignments().unwrap(), original);
        let mut bad = c.clone();
        bad.hardware.chain_assignments.as_mut().unwrap().reward = "serial-c".into();
        assert!(bad.validate().is_err());
        bad = c.clone();
        bad.hardware.chain_assignments.as_mut().unwrap().reward = "missing".into();
        assert!(bad.validate().is_err());
        bad = c.clone();
        bad.hardware.boards.pop();
        assert!(bad.validate().is_err());
        bad = c.clone();
        bad.hardware.chain_assignments = None;
        bad.hardware.boards[1].serial = bad.hardware.boards[0].serial.clone();
        assert!(bad.validate().is_err());
        let encoded = toml::to_string(&c).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&encoded)
                .unwrap()
                .assignments()
                .unwrap(),
            original
        );
    }
    #[test]
    fn one_board_has_three_distinct_engines() {
        let mut c = Config::default();
        c.hardware.boards.push(board("a", 18110));
        let p = c.assignments().unwrap();
        assert_eq!(
            p.iter().map(|a| a.engine).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(p.iter().all(|a| a.board_id == "a"));
    }
    #[test]
    fn three_boards_get_one_chain_each() {
        let mut c = Config::default();
        c.hardware.boards = vec![board("a", 18110), board("b", 18111), board("c", 18112)];
        let p = c.assignments().unwrap();
        assert!(p.iter().all(|a| a.engine == 1));
        assert_eq!(p[2].board_id, "c");
        assert_eq!(p[2].chain, Chain::InfusedChallenge);
    }
    #[test]
    fn two_boards_share_all_three_engines() {
        let mut c = Config::default();
        c.hardware.boards = vec![board("a", 18110), board("b", 18111)];
        let plan = c.assignments().unwrap();
        assert_eq!(plan.iter().filter(|a| a.board_id == "a").count(), 2);
        assert_eq!(plan[2].board_id, "b");
        assert_eq!(
            plan.iter().map(|a| a.engine).collect::<Vec<_>>(),
            vec![0, 1, 1]
        );
    }
    #[test]
    fn rejects_ambiguous_devices_and_ports() {
        let mut c = Config::default();
        c.hardware.boards = vec![board("a", 18110), board("b", 18110)];
        assert!(c.validate().is_err());
        c.hardware.boards[1].client_port = 18111;
        c.hardware.boards[0].device_location = Some(0);
        assert!(c.validate().is_err());
    }
    #[test]
    fn rejects_public_status_and_partial_credentials() {
        let mut c = Config::default();
        c.service.status_listen = "0.0.0.0:9257".parse().unwrap();
        assert!(c.validate().is_err());
        c.service = ServiceConfig::default();
        c.node.private_key = Some("secret.key".into());
        assert!(c.validate().is_err());
    }
    #[test]
    fn managed_clients_require_explicit_devices_and_preserve_prototype_limits() {
        let mut config = Config::default();
        config.hardware.boards.push(board("a", 18110));
        config.hardware.client_binary = Some("/native/hw_vdf_client".into());
        assert!(config.validate().is_err());
        config.hardware.boards[0].device_location = Some(12305);
        assert!(config.validate().is_ok());
        config.hardware.boards[0].voltage_v = 1.01;
        assert!(config.validate().is_err());
        config.hardware.boards[0].voltage_v = 1.0;
        config.hardware.boards[0].temperature_limit_c = 56.0;
        assert!(config.validate().is_err());
        config.hardware.boards[0].temperature_limit_c = 55.0;
        config.hardware.boards[0].frequency_mhz = f64::NAN;
        assert!(config.validate().is_err());
    }
    #[test]
    fn automatic_frequency_ceiling_preserves_voltage_and_temperature_limits() {
        let mut c: Config =
            toml::from_str(include_str!("../../../config/three-boards.toml")).unwrap();
        let b = &mut c.hardware.boards[0];
        b.automatic.max_frequency_mhz = 2200.;
        b.automatic.start_frequency_mhz = 2200.;
        assert!(c.validate().is_ok());
        c.hardware.boards[0].automatic.max_frequency_mhz = 2201.;
        assert!(c.validate().is_err());
        c.hardware.boards[0].automatic.max_frequency_mhz = 2200.;
        c.hardware.boards[0].automatic.start_voltage_v = 1.01;
        assert!(c.validate().is_err());
    }
    #[test]
    fn supplied_examples_parse_and_validate() {
        for s in [
            include_str!("../../../config/development.toml"),
            include_str!("../../../config/three-boards.toml"),
        ] {
            let c: Config = toml::from_str(s).unwrap();
            c.validate().unwrap();
            assert_eq!(c.assignments().unwrap().len(), 3);
        }
        assert!(toml::from_str::<Config>("schema_version=1\nmisspelled_setting=true").is_err());
    }
}
