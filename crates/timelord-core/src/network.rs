use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Exactly 32 bytes of hexadecimal text, as Chia writes hashes.
pub fn is_hex_32(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Integers use an explicit decimal representation across JSON, TOML and Swift.
/// Unlike floating point or TOML integers, this covers the full u128 range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExactInteger {
    integer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExactDecimal {
    decimal: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum NetworkValue {
    Integer(ExactInteger),
    Decimal(ExactDecimal),
    String(String),
    Bool(bool),
    Array(Vec<NetworkValue>),
    Object(BTreeMap<String, NetworkValue>),
}
impl NetworkValue {
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn unsigned<T: TryFrom<u128>>(&self) -> Result<T, String> {
        if let Self::Integer(value) = self {
            value
                .integer
                .parse::<u128>()
                .ok()
                .and_then(|v| T::try_from(v).ok())
                .ok_or_else(|| "Network integer is outside the supported range".into())
        } else {
            Err("Expected an integer network constant".into())
        }
    }
}
impl From<String> for NetworkValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<i32> for NetworkValue {
    fn from(value: i32) -> Self {
        Self::Integer(ExactInteger {
            integer: value.to_string(),
        })
    }
}
impl<'de> Deserialize<'de> for NetworkValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        fn convert(value: serde_json::Value) -> Result<NetworkValue, String> {
            use serde_json::Value;
            Ok(match value {
                // TOML cannot represent null, and Chia never exports it in constants.
                Value::Null => return Err("Network constant values cannot be null".into()),
                Value::Number(v) => {
                    let integer = v.to_string();
                    if integer.parse::<u128>().is_err() && integer.parse::<i128>().is_err() {
                        return Ok(NetworkValue::Decimal(ExactDecimal { decimal: integer }));
                    }
                    NetworkValue::Integer(ExactInteger { integer })
                }
                Value::String(v) => NetworkValue::String(v),
                Value::Bool(v) => NetworkValue::Bool(v),
                Value::Array(v) => {
                    NetworkValue::Array(v.into_iter().map(convert).collect::<Result<_, _>>()?)
                }
                Value::Object(v) if v.len() == 1 && v.contains_key("integer") => {
                    let value: ExactInteger =
                        serde_json::from_value(Value::Object(v)).map_err(|e| e.to_string())?;
                    // Normalize canonical integer text, rejecting ambiguous encodings.
                    let text = &value.integer;
                    let canonical = text
                        .parse::<u128>()
                        .map(|n| n.to_string())
                        .or_else(|_| text.parse::<i128>().map(|n| n.to_string()))
                        .map_err(|_| "Invalid exact network integer")?;
                    if canonical != *text {
                        return Err("Noncanonical network integer".into());
                    }
                    NetworkValue::Integer(value)
                }
                Value::Object(v) if v.len() == 1 && v.contains_key("decimal") => {
                    let value: ExactDecimal =
                        serde_json::from_value(Value::Object(v)).map_err(|e| e.to_string())?;
                    value
                        .decimal
                        .parse::<serde_json::Number>()
                        .map_err(|e| e.to_string())?;
                    NetworkValue::Decimal(value)
                }
                Value::Object(v) => NetworkValue::Object(
                    v.into_iter()
                        .map(|(k, v)| Ok((k, convert(v)?)))
                        .collect::<Result<_, String>>()?,
                ),
            })
        }
        convert(serde_json::Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Consensus identity excludes export time, software version and presentation.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChainIdentity {
    network: Network,
    constants: Option<BTreeMap<String, NetworkValue>>,
}
impl ChainIdentity {
    pub fn new(network: &Network, definition: Option<&NetworkDefinition>) -> Result<Self, String> {
        let constants = match (network, definition) {
            (Network::Custom(_), Some(definition)) => {
                definition.validate(network)?;
                Some(definition.constants.clone())
            }
            (Network::Custom(_), None) => {
                return Err("Custom networks require an imported network definition".into());
            }
            (_, Some(_)) => {
                return Err("Built-in networks cannot use a custom definition".into());
            }
            (_, None) => None,
        };
        Ok(Self {
            network: network.clone(),
            constants,
        })
    }
    /// The validated constants of a custom chain; `None` for built-in networks.
    pub fn constants(&self) -> Option<&BTreeMap<String, NetworkValue>> {
        self.constants.as_ref()
    }
}

/// A validated Chia `selected_network` ID for a chain without a built-in definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub struct CustomNetworkId(String);
impl CustomNetworkId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for CustomNetworkId {
    type Error = String;
    fn try_from(id: String) -> Result<Self, Self::Error> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("Network ID must be 1–64 lowercase letters, digits or hyphens".into());
        }
        if matches!(id.as_str(), "mainnet" | "testnet11") {
            return Err(format!(
                "Network ID {id} is reserved for a built-in network"
            ));
        }
        Ok(Self(id))
    }
}
impl std::fmt::Display for CustomNetworkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl From<CustomNetworkId> for String {
    fn from(id: CustomNetworkId) -> Self {
        id.0
    }
}

/// Wire network IDs remain strings in configuration and status JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub enum Network {
    #[default]
    Mainnet,
    Testnet11,
    Custom(CustomNetworkId),
}
impl Network {
    pub fn label(&self) -> &str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet11 => "testnet11",
            Self::Custom(id) => id.as_str(),
        }
    }
}
impl TryFrom<String> for Network {
    type Error = String;
    fn try_from(name: String) -> Result<Self, Self::Error> {
        Ok(match name.as_str() {
            "mainnet" => Self::Mainnet,
            "testnet11" => Self::Testnet11,
            _ => Self::Custom(name.try_into()?),
        })
    }
}
impl From<Network> for String {
    fn from(network: Network) -> Self {
        network.label().to_owned()
    }
}
impl std::str::FromStr for Network {
    type Err = String;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        name.to_owned().try_into()
    }
}
impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Non-secret, portable definition exported from the selected Chia root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkDefinition {
    pub schema_version: u32,
    pub network_id: String,
    pub exported_at: String,
    pub chia_version: String,
    pub constants: BTreeMap<String, NetworkValue>,
    #[serde(default)]
    pub config_overrides: BTreeMap<String, NetworkValue>,
}
impl NetworkDefinition {
    pub fn validate(&self, network: &Network) -> Result<(), String> {
        if self.schema_version != 1 || self.network_id != network.label() {
            return Err("Network definition version or network ID does not match".into());
        }
        if self.constants.keys().any(|key| {
            key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        }) {
            return Err(
                "Network constant names must use uppercase letters, digits or underscores".into(),
            );
        }
        if !self
            .constants
            .get("GENESIS_CHALLENGE")
            .and_then(NetworkValue::as_str)
            .is_some_and(is_hex_32)
        {
            return Err("Custom network requires a 32-byte hex GENESIS_CHALLENGE".into());
        }
        Ok(())
    }
}

/// Node-level entry to the shared chain identity rule: a custom chain needs a
/// definition and a built-in chain must not carry one. Partial network intent
/// (last-good fallback) uses [`ChainIdentity::new`] directly.
impl crate::NodeConfig {
    pub fn chain_identity(&self) -> Result<ChainIdentity, String> {
        ChainIdentity::new(&self.network, self.network_definition.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_ids_are_validated_and_never_shadow_built_in_networks() {
        for name in ["ephemeral-fixture", "testnet0", "a", &"n".repeat(64)] {
            let network: Network = name.parse().unwrap();
            assert_eq!(network.label(), name);
            assert_eq!(String::from(network.clone()), name);
            assert_eq!(
                serde_json::to_string(&network).unwrap(),
                format!("\"{name}\"")
            );
            let Network::Custom(id) = network else {
                panic!("{name} must stay custom");
            };
            assert_eq!(id.as_str(), name);
        }
        for name in ["", "Mainnet", "test net", "test.net", &"n".repeat(65)] {
            assert!(name.parse::<Network>().is_err(), "{name}");
        }
        // Built-in names still resolve to built-ins, not custom IDs.
        assert_eq!("mainnet".parse::<Network>().unwrap(), Network::Mainnet);
        assert_eq!("testnet11".parse::<Network>().unwrap(), Network::Testnet11);
        assert!(CustomNetworkId::try_from("mainnet".to_owned()).is_err());
        assert!(CustomNetworkId::try_from("testnet11".to_owned()).is_err());
    }

    #[test]
    fn network_values_round_trip_exactly_and_reject_null() {
        let value: NetworkValue = serde_json::from_str(
            r#"{"FACTOR":147573952589676412928,"FLAG":true,"NAME":"xch","LIST":[1,2],"NESTED":{"a":"b"}}"#,
        )
        .unwrap();
        let text = serde_json::to_string(&value).unwrap();
        let restored: NetworkValue = serde_json::from_str(&text).unwrap();
        assert_eq!(restored, value);
        for invalid in ["null", r#"{"A":null}"#, r#"{"A":[1,null]}"#] {
            assert!(
                serde_json::from_str::<NetworkValue>(invalid).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            serde_json::from_str::<NetworkValue>("184467440737095516160000")
                .unwrap()
                .unsigned::<u128>()
                .unwrap(),
            184467440737095516160000
        );
    }

    #[test]
    fn chain_identity_is_the_one_custom_definition_rule() {
        let mut node = crate::NodeConfig::default();
        node.chain_identity().unwrap();
        node.network = "ephemeral-fixture".parse().unwrap();
        assert!(node.chain_identity().is_err());
        node.network_definition = Some(definition("ephemeral-fixture"));
        node.chain_identity().unwrap();
        node.network = Network::Mainnet;
        assert!(node.chain_identity().is_err());
    }

    #[test]
    fn definition_requires_a_32_byte_hex_genesis_challenge() {
        let network: Network = "ephemeral-fixture".parse().unwrap();
        let mut definition = definition("ephemeral-fixture");
        definition
            .constants
            .insert("GENESIS_CHALLENGE".into(), "ab".repeat(32).into());
        definition.validate(&network).unwrap();
        for invalid in ["", "ab", &"zz".repeat(32), &format!("{}0", "ab".repeat(32))] {
            definition
                .constants
                .insert("GENESIS_CHALLENGE".into(), invalid.to_owned().into());
            assert!(definition.validate(&network).is_err(), "{invalid}");
        }
        definition
            .constants
            .insert("GENESIS_CHALLENGE".into(), "ab".repeat(32).into());
        definition.constants.insert("bad name".into(), 1.into());
        assert!(definition.validate(&network).is_err());
    }

    fn definition(network_id: &str) -> NetworkDefinition {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1, "network_id": network_id,
            "exported_at": "fixture", "chia_version": "fixture",
            "constants": {"GENESIS_CHALLENGE": "ab".repeat(32)},
        }))
        .unwrap()
    }
}
