use serde::{Deserialize, Serialize};

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
    Object(std::collections::BTreeMap<String, NetworkValue>),
    Null,
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
                Value::Number(v) => {
                    let integer = v.to_string();
                    if integer.parse::<u128>().is_err() && integer.parse::<i128>().is_err() {
                        return Ok(NetworkValue::Decimal(ExactDecimal { decimal: integer }));
                    }
                    NetworkValue::Integer(ExactInteger { integer })
                }
                Value::String(v) => NetworkValue::String(v),
                Value::Bool(v) => NetworkValue::Bool(v),
                Value::Null => NetworkValue::Null,
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
    constants: Option<std::collections::BTreeMap<String, NetworkValue>>,
}
impl ChainIdentity {
    pub fn new(network: &Network, definition: Option<&NetworkDefinition>) -> Result<Self, String> {
        let constants = match (network, definition) {
            (Network::Custom(_), Some(definition)) => {
                definition.validate(network)?;
                Some(definition.constants.clone())
            }
            (Network::Custom(_), None) => return Err("Missing custom chain definition".into()),
            (_, Some(_)) => return Err("Built-in network cannot use a custom definition".into()),
            (_, None) => None,
        };
        Ok(Self {
            network: network.clone(),
            constants,
        })
    }
}

/// Wire network IDs remain strings in configuration and status JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub enum Network {
    #[default]
    Mainnet,
    Testnet11,
    Custom(String),
}
impl Network {
    pub fn label(&self) -> &str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet11 => "testnet11",
            Self::Custom(name) => name,
        }
    }
}
impl TryFrom<String> for Network {
    type Error = String;
    fn try_from(name: String) -> Result<Self, Self::Error> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("Network ID must be 1–64 lowercase letters, digits or hyphens".into());
        }
        Ok(match name.as_str() {
            "mainnet" => Self::Mainnet,
            "testnet11" => Self::Testnet11,
            _ => Self::Custom(name),
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
    pub constants: std::collections::BTreeMap<String, NetworkValue>,
    #[serde(default)]
    pub config_overrides: std::collections::BTreeMap<String, NetworkValue>,
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
        let genesis = self
            .constants
            .get("GENESIS_CHALLENGE")
            .and_then(|v| v.as_str());
        if !genesis
            .is_some_and(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("Custom network requires a 32-byte hex GENESIS_CHALLENGE".into());
        }
        Ok(())
    }
}
