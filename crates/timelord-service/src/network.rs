//! Resolve Chia network definitions without changing the pinned scheduler.
use anyhow::{Context, Result, bail};
use dg_xch_core::consensus::{
    chain_definition::{ChainSelection, ResolvedChain},
    constants::{ConsensusConstants, MAINNET},
};
use std::collections::BTreeMap;
use timelord_core::{Network, NetworkValue, NodeConfig};

/// Chia consensus/presentation keys the pinned scheduler derives itself or does
/// not model. A real exported `DEFAULT_CONSTANTS` file carries them; accept them
/// with their types checked, but never silently ignore arbitrary names.
const UNUSED_CHIA_CONSTANTS: &[&str] = &[
    "AGG_SIG_PARENT_ADDITIONAL_DATA",
    "AGG_SIG_PUZZLE_ADDITIONAL_DATA",
    "AGG_SIG_AMOUNT_ADDITIONAL_DATA",
    "AGG_SIG_PUZZLE_AMOUNT_ADDITIONAL_DATA",
    "AGG_SIG_PARENT_AMOUNT_ADDITIONAL_DATA",
    "AGG_SIG_PARENT_PUZZLE_ADDITIONAL_DATA",
    "BECH32M_PREFIX",
];

pub fn resolve(node: &NodeConfig) -> Result<ResolvedChain> {
    let identity = node.chain_identity().map_err(anyhow::Error::msg)?;
    let Network::Custom(_) = &node.network else {
        return ChainSelection::from_config(node.network.label(), None)
            .and_then(|chain| chain.resolve())
            .map_err(anyhow::Error::msg);
    };
    let definitions = identity
        .constants()
        .context("Custom network definition has no constants")?;
    let mut constants = MAINNET;
    constants.is_testnet = true;
    apply(&mut constants, definitions)?;
    // Reuse the pinned scheduler's pure constructor to validate all parameters
    // before an import/configuration can replace usable saved settings.
    dg_xch_timelord::scheduler::Scheduler::new(constants, false, None)
        .context("Invalid custom network scheduler parameters")?;
    // Custom Chia networks are not the upstream DGX chain format: their handshake
    // ID is the exact selected_network, with no genesis suffix or derived seed.
    Ok(ResolvedChain {
        constants,
        network_id: node.network.label().into(),
        handshake_network_id: node.network.label().into(),
        allows_bootstrap: false,
    })
}

/// Apply every exported constant, rejecting unknown names and alias collisions.
fn apply(
    constants: &mut ConsensusConstants,
    definitions: &BTreeMap<String, NetworkValue>,
) -> Result<()> {
    let mut claimed: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, value) in definitions {
        let field = canonical_name(name);
        if let Some(previous) = claimed.insert(field, name) {
            bail!("Network constants {previous} and {name} are aliases; supply only one");
        }
        if UNUSED_CHIA_CONSTANTS.contains(&field) {
            unused_constant(field, value)?;
            continue;
        }
        set(constants, field, name, value)?;
    }
    Ok(())
}

/// Legacy Chia names share the value of their current name.
fn canonical_name(name: &str) -> &str {
    match name {
        "MIN_PLOT_SIZE_V1" => "MIN_PLOT_SIZE",
        "MAX_PLOT_SIZE_V1" => "MAX_PLOT_SIZE",
        "NUMBER_ZERO_BITS_PLOT_FILTER_V1" => "NUMBER_ZERO_BITS_PLOT_FILTER",
        // `NETWORK_TYPE` is Chia's pre-`TESTNET` spelling of the same flag.
        "NETWORK_TYPE" => "TESTNET",
        _ => name,
    }
}

fn unused_constant(name: &str, value: &NetworkValue) -> Result<()> {
    if name == "BECH32M_PREFIX" {
        value
            .as_str()
            .filter(|value| !value.is_empty())
            .context("BECH32M_PREFIX must be a nonempty string")?;
        return Ok(());
    }
    let text = value.as_str().context("Expected a 32-byte hex value")?;
    if !timelord_core::is_hex_32(text) {
        bail!("Network constant {name} must be 32 bytes of hex");
    }
    Ok(())
}

fn set(
    constants: &mut ConsensusConstants,
    name: &str,
    source: &str,
    value: &NetworkValue,
) -> Result<()> {
    macro_rules! numeric {
        ($($field:ident),* $(,)?) => {
            $(
                if name.eq_ignore_ascii_case(stringify!($field)) {
                    constants.$field = value.unsigned().map_err(anyhow::Error::msg)
                        .with_context(|| format!("Invalid network constant {source}"))?;
                    return Ok(());
                }
            )*
        };
    }
    numeric!(
        slot_blocks_target,
        min_blocks_per_challenge_block,
        max_sub_slot_blocks,
        num_sps_sub_slot,
        sub_slot_iters_starting,
        difficulty_constant_factor,
        difficulty_starting,
        difficulty_change_max_factor,
        sub_epoch_blocks,
        epoch_blocks,
        significant_bits,
        discriminant_size_bits,
        number_zero_bits_plot_filter,
        min_plot_size,
        max_plot_size,
        sub_slot_time_target,
        num_sp_intervals_extra,
        max_future_time,
        max_future_time2,
        number_of_timestamps,
        max_vdf_witness_size,
        mempool_block_buffer,
        max_coin_amount,
        max_block_cost_clvm,
        cost_per_byte,
        weight_proof_threshold,
        weight_proof_recent_blocks,
        max_block_count_per_requests,
        blocks_cache_size,
        max_generator_size,
        max_generator_ref_list_size,
        pool_sub_slot_iters,
        soft_fork2_height,
        soft_fork3_height,
        soft_fork8_height,
        soft_fork9_height,
        hard_fork_height,
        hard_fork_fix_height,
        hard_fork2_height,
        plot_filter_128_height,
        plot_filter_64_height,
        plot_filter_32_height,
        number_zero_bits_plot_filter_v2,
        plot_size_v2,
        filter_window_size,
        max_effective_plot_filter_bits,
        min_plot_strength,
        max_plot_strength,
        plot_v1_phase_out_epoch_bits,
        plot_filter_v2_first_adjustment_height,
        plot_filter_v2_second_adjustment_height,
        plot_filter_v2_third_adjustment_height,
    );
    match name {
        "PLOT_FILTER_V2_RELATIVE_HEIGHT" => {
            let NetworkValue::Array(values) = value else {
                bail!("Network constant {source} must be a list of relative heights");
            };
            let entries: Vec<u32> = values
                .iter()
                .map(|value| value.unsigned().map_err(anyhow::Error::msg))
                .collect::<Result<_>>()
                .with_context(|| format!("Invalid network constant {source}"))?;
            constants.plot_filter_v2_relative_height =
                dg_xch_core::consensus::filter_schedule::FilterSchedule::try_from_slice(&entries)
                    .with_context(|| format!("Invalid network constant {source}"))?;
        }
        "GENESIS_CHALLENGE" => constants.genesis_challenge = hex_constant(source, value)?,
        "AGG_SIG_ME_ADDITIONAL_DATA" => {
            constants.agg_sig_me_additional_data = hex_constant(source, value)?;
        }
        "GENESIS_PRE_FARM_POOL_PUZZLE_HASH" => {
            constants.genesis_pre_farm_pool_puzzle_hash = hex_constant(source, value)?;
        }
        "GENESIS_PRE_FARM_FARMER_PUZZLE_HASH" => {
            constants.genesis_pre_farm_farmer_puzzle_hash = hex_constant(source, value)?;
        }
        "TESTNET" => constants.is_testnet = testnet(source, value)?,
        _ => bail!("Unknown network constant {source}; only Chia consensus constants are accepted"),
    }
    Ok(())
}

fn testnet(name: &str, value: &NetworkValue) -> Result<bool> {
    match value {
        NetworkValue::Bool(value) => Ok(*value),
        _ => match value.unsigned::<u64>().map_err(anyhow::Error::msg)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => bail!("Network constant {name} must be 0, 1 or a boolean"),
        },
    }
}

fn hex_constant(
    name: &str,
    value: &NetworkValue,
) -> Result<dg_xch_core::blockchain::sized_bytes::Bytes32> {
    use std::str::FromStr;
    let text = value.as_str().context("Expected a 32-byte hex value")?;
    if !timelord_core::is_hex_32(text) {
        bail!("Network constant {name} must be 32 bytes of hex");
    }
    dg_xch_core::blockchain::sized_bytes::Bytes32::from_str(text).map_err(anyhow::Error::msg)
}

pub fn yaml_overrides(
    value: &yaml_rust2::Yaml,
) -> Result<std::collections::BTreeMap<String, timelord_core::NetworkValue>> {
    fn json(value: &yaml_rust2::Yaml) -> Result<serde_json::Value> {
        use yaml_rust2::Yaml;
        Ok(match value {
            Yaml::String(v) => v.clone().into(),
            Yaml::Integer(v) => (*v).into(),
            Yaml::Real(v) => serde_json::from_str(v).context("Invalid numeric network override")?,
            Yaml::Boolean(v) => (*v).into(),
            Yaml::Array(v) => serde_json::Value::Array(v.iter().map(json).collect::<Result<_>>()?),
            Yaml::Hash(v) => serde_json::Value::Object(
                v.iter()
                    .map(|(k, v)| {
                        Ok((
                            k.as_str()
                                .context("Network override keys must be strings")?
                                .to_owned(),
                            json(v)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
            _ => bail!("Unsupported network override value; TOML cannot represent null"),
        })
    }
    if value.is_badvalue() {
        return Ok(Default::default());
    }
    serde_json::from_value(json(value)?).map_err(Into::into)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn node() -> NodeConfig {
        NodeConfig { network: "ephemeral-fixture".parse().unwrap(),
        network_definition: Some(serde_json::from_value(serde_json::json!({
            "schema_version": 1, "network_id": "ephemeral-fixture",
            "exported_at": "fixture", "chia_version": "fixture",
            "constants": {"GENESIS_CHALLENGE": "ab".repeat(32),
                "DIFFICULTY_STARTING": 250, "DIFFICULTY_CONSTANT_FACTOR": 10052721566054u64,
                "SUB_SLOT_ITERS_STARTING": 33554432, "HARD_FORK2_HEIGHT": 9216,
                "MIN_PLOT_SIZE": 18, "NETWORK_TYPE": 1,
                "PLOT_FILTER_V2_RELATIVE_HEIGHT": [33000, 30000, 27000, 24000, 21000, 18000, 9216]},
            "config_overrides": {"address_prefix": "txch"}
        })).unwrap()), ..Default::default() }
    }
    fn with_constants(constants: serde_json::Value) -> NodeConfig {
        serde_json::from_value(serde_json::json!({
            "host": "localhost", "port": 8444, "network": "ephemeral-fixture",
            "network_definition": {
                "schema_version": 1, "network_id": "ephemeral-fixture",
                "exported_at": "fixture", "chia_version": "fixture",
                "constants": constants,
            }
        }))
        .unwrap()
    }
    #[test]
    fn custom_id_and_exact_constants_round_trip_without_dgx_derivation() {
        let node = node();
        let config = timelord_core::Config {
            node: node.clone(),
            ..Default::default()
        };
        config.validate().unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        let restored: timelord_core::Config = toml::from_str(&text).unwrap();
        assert_eq!(restored.node.network_definition, node.network_definition);
        let resolved = resolve(&restored.node).unwrap();
        assert_eq!(resolved.handshake_network_id, node.network.label());
        assert_eq!(
            resolved
                .constants
                .genesis_challenge
                .to_string()
                .trim_start_matches("0x"),
            "ab".repeat(32)
        );
        assert_eq!(resolved.constants.difficulty_starting, 250);
        assert_eq!(
            resolved.constants.difficulty_constant_factor,
            10052721566054
        );
        assert_eq!(resolved.constants.sub_slot_iters_starting, 33554432);
        assert_eq!(resolved.constants.hard_fork2_height, 9216);
        assert_eq!(
            resolved
                .constants
                .plot_filter_v2_relative_height
                .entries()
                .len(),
            7
        );
        assert_eq!(
            resolved.constants.plot_filter_v2_relative_height.entries(),
            &[33000, 30000, 27000, 24000, 21000, 18000, 9216]
        );
        assert!(!resolved.allows_bootstrap);
    }

    #[test]
    fn full_chia_default_constants_import_cleanly() {
        // A real exported network.json: every key chia-blockchain's
        // DEFAULT_CONSTANTS carries today, including presentation and derived
        // AGG_SIG values the pinned scheduler does not consume.
        let node: NodeConfig = serde_json::from_str(
            r#"{
            "host": "localhost", "port": 8444, "network": "ephemeral-fixture",
            "network_definition": {
                "schema_version": 1, "network_id": "ephemeral-fixture",
                "exported_at": "fixture", "chia_version": "fixture",
                "constants": {
                    "SLOT_BLOCKS_TARGET": 32,
                    "MIN_BLOCKS_PER_CHALLENGE_BLOCK": 16,
                    "MAX_SUB_SLOT_BLOCKS": 128,
                    "NUM_SPS_SUB_SLOT": 64,
                    "SUB_SLOT_ITERS_STARTING": 134217728,
                    "DIFFICULTY_CONSTANT_FACTOR": 147573952589676412928,
                    "DIFFICULTY_STARTING": 7,
                    "DIFFICULTY_CHANGE_MAX_FACTOR": 3,
                    "SUB_EPOCH_BLOCKS": 384,
                    "EPOCH_BLOCKS": 4608,
                    "SIGNIFICANT_BITS": 8,
                    "DISCRIMINANT_SIZE_BITS": 1024,
                    "NUMBER_ZERO_BITS_PLOT_FILTER_V1": 9,
                    "NUMBER_ZERO_BITS_PLOT_FILTER_V2": 9,
                    "MIN_PLOT_SIZE_V1": 32,
                    "MAX_PLOT_SIZE_V1": 50,
                    "PLOT_SIZE_V2": 28,
                    "SUB_SLOT_TIME_TARGET": 600,
                    "NUM_SP_INTERVALS_EXTRA": 3,
                    "MAX_FUTURE_TIME2": 120,
                    "NUMBER_OF_TIMESTAMPS": 11,
                    "GENESIS_CHALLENGE": "ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb",
                    "AGG_SIG_ME_ADDITIONAL_DATA": "ccd5bb71183532bff220ba46c268991a3ff07eb358e8255a65c30a2dce0e5fbb",
                    "AGG_SIG_PARENT_ADDITIONAL_DATA": "baf5d69c647c91966170302d18521b0a85663433d161e72c826ed08677b53a74",
                    "AGG_SIG_PUZZLE_ADDITIONAL_DATA": "284fa2ef486c7a41cc29fc99c9d08376161e93dd37817edb8219f42dca7592c4",
                    "AGG_SIG_AMOUNT_ADDITIONAL_DATA": "cda186a9cd030f7a130fae45005e81cae7a90e0fa205b75f6aebc0d598e0348e",
                    "AGG_SIG_PUZZLE_AMOUNT_ADDITIONAL_DATA": "0f7d90dff0613e6901e24dae59f1e690f18b8f5fbdcf1bb192ac9deaf7de22ad",
                    "AGG_SIG_PARENT_AMOUNT_ADDITIONAL_DATA": "585796bd90bb553c0430b87027ffee08d88aba0162c6e1abbbcc6b583f2ae7f9",
                    "AGG_SIG_PARENT_PUZZLE_ADDITIONAL_DATA": "2ebfdae17b29d83bae476a25ea06f0c4bd57298faddbbc3ec5ad29b9b86ce5df",
                    "GENESIS_PRE_FARM_POOL_PUZZLE_HASH": "d23da14695a188ae5708dd152263c4db883eb27edeb936178d4d988b8f3ce5fc",
                    "GENESIS_PRE_FARM_FARMER_PUZZLE_HASH": "3d8765d3a597ec1d99663f6c9816d915b9f68613ac94009884c4addaefcce6af",
                    "MAX_VDF_WITNESS_SIZE": 64,
                    "MEMPOOL_BLOCK_BUFFER": 10,
                    "MAX_COIN_AMOUNT": 18446744073709551615,
                    "MAX_BLOCK_COST_CLVM": 11000000000,
                    "COST_PER_BYTE": 12000,
                    "WEIGHT_PROOF_THRESHOLD": 2,
                    "BLOCKS_CACHE_SIZE": 5120,
                    "WEIGHT_PROOF_RECENT_BLOCKS": 1000,
                    "MAX_BLOCK_COUNT_PER_REQUESTS": 32,
                    "MAX_GENERATOR_REF_LIST_SIZE": 512,
                    "POOL_SUB_SLOT_ITERS": 37600000000,
                    "HARD_FORK_HEIGHT": 5496000,
                    "HARD_FORK2_HEIGHT": 4294967290,
                    "SOFT_FORK8_HEIGHT": 8655000,
                    "SOFT_FORK9_HEIGHT": 8655000,
                    "PLOT_V1_PHASE_OUT_EPOCH_BITS": 8,
                    "PLOT_FILTER_128_HEIGHT": 10542000,
                    "PLOT_FILTER_64_HEIGHT": 15592000,
                    "PLOT_FILTER_32_HEIGHT": 20643000,
                    "MIN_PLOT_STRENGTH": 2,
                    "MAX_PLOT_STRENGTH": 17,
                    "PLOT_FILTER_V2_RELATIVE_HEIGHT": [
                        50494000, 45444000, 40394000, 35343000, 30298000, 25247000,
                        20197000, 15146000, 10101000
                    ],
                    "FILTER_WINDOW_SIZE": 16,
                    "MAX_EFFECTIVE_PLOT_FILTER_BITS": 13,
                    "TESTNET": false,
                    "BECH32M_PREFIX": "xch"
                }
            }
        }"#,
        )
        .unwrap();
        let resolved = resolve(&node).unwrap();
        let constants = resolved.constants;
        assert!(!constants.is_testnet);
        assert_eq!(constants.max_generator_size, 1_000_000);
        assert_eq!(constants.max_future_time, 300);
        assert_eq!(constants.number_of_timestamps, 11);
        assert_eq!(constants.soft_fork2_height, 0);
        assert_eq!(constants.soft_fork3_height, 4_510_000);
        assert_eq!(constants.hard_fork_fix_height, 5_496_000);
        assert_eq!(constants.max_effective_plot_filter_bits, 13);
        assert_eq!(constants.filter_window_size, 16);
        assert_eq!(constants.min_plot_strength, 2);
        assert_eq!(constants.max_plot_strength, 17);
        assert_eq!(constants.min_plot_size, 32);
        assert_eq!(constants.max_plot_size, 50);
        assert_eq!(constants.number_zero_bits_plot_filter, 9);
        assert_eq!(
            constants.plot_filter_v2_relative_height.entries(),
            &[
                50_494_000, 45_444_000, 40_394_000, 35_343_000, 30_298_000, 25_247_000, 20_197_000,
                15_146_000, 10_101_000
            ]
        );
    }

    #[test]
    fn legacy_chia_constants_and_aliases_import_cleanly() {
        let constants = serde_json::json!({
            "GENESIS_CHALLENGE": "ab".repeat(32),
            "MIN_PLOT_SIZE": 18,
            "MAX_PLOT_SIZE": 50,
            "NUMBER_ZERO_BITS_PLOT_FILTER": 7,
            "NETWORK_TYPE": 1,
            "MAX_FUTURE_TIME": 300,
            "MAX_GENERATOR_SIZE": 1_000_000,
            "SOFT_FORK2_HEIGHT": 0,
            "SOFT_FORK3_HEIGHT": 4_510_000,
            "HARD_FORK_FIX_HEIGHT": 5_496_000,
        });
        let resolved = resolve(&with_constants(constants)).unwrap();
        assert!(resolved.constants.is_testnet);
        assert_eq!(resolved.constants.min_plot_size, 18);
        assert_eq!(resolved.constants.max_plot_size, 50);
        assert_eq!(resolved.constants.number_zero_bits_plot_filter, 7);
        assert_eq!(resolved.constants.max_future_time, 300);
        assert_eq!(resolved.constants.max_generator_size, 1_000_000);

        // The modern spellings carry the same values, but never both at once.
        let modern = serde_json::json!({
            "GENESIS_CHALLENGE": "ab".repeat(32),
            "MIN_PLOT_SIZE_V1": 18,
            "MAX_PLOT_SIZE_V1": 50,
            "NUMBER_ZERO_BITS_PLOT_FILTER_V1": 7,
            "TESTNET": true,
        });
        let resolved = resolve(&with_constants(modern)).unwrap();
        assert_eq!(resolved.constants.min_plot_size, 18);
        assert!(resolved.constants.is_testnet);
    }

    #[test]
    fn built_in_networks_cannot_resolve_with_a_custom_definition() {
        let mut node = node();
        node.network = "mainnet".parse().unwrap();
        assert!(resolve(&node).is_err());
        node.network_definition = None;
        assert_eq!(resolve(&node).unwrap().handshake_network_id, "mainnet");
    }

    #[test]
    fn unknown_misspelled_and_aliased_constants_fail_loudly() {
        for (key, value) in [
            ("DIFFICULTY_STARTNG", 250.into()),
            ("MIN_PLOT_SIZE_V1X", 18.into()),
            ("AGG_SIG_ME_ADDITIONAL_DAT", "ab".repeat(32).into()),
        ] {
            let mut node = node();
            node.network_definition
                .as_mut()
                .unwrap()
                .constants
                .insert(key.into(), value);
            let error = resolve(&node).unwrap_err().to_string();
            assert!(error.contains("Unknown network constant"), "{key}: {error}");
        }
        for (first, second) in [
            ("MIN_PLOT_SIZE", "MIN_PLOT_SIZE_V1"),
            ("MAX_PLOT_SIZE", "MAX_PLOT_SIZE_V1"),
            (
                "NUMBER_ZERO_BITS_PLOT_FILTER",
                "NUMBER_ZERO_BITS_PLOT_FILTER_V1",
            ),
            ("NETWORK_TYPE", "TESTNET"),
        ] {
            let mut node = node();
            let constants = &mut node.network_definition.as_mut().unwrap().constants;
            constants.insert(first.into(), 1.into());
            constants.insert(second.into(), 1.into());
            let error = resolve(&node).unwrap_err().to_string();
            assert!(error.contains("aliases"), "{first}/{second}: {error}");
        }
        let mut node = node();
        node.network_definition.as_mut().unwrap().constants.insert(
            "AGG_SIG_PARENT_ADDITIONAL_DATA".into(),
            "not hex".to_owned().into(),
        );
        assert!(resolve(&node).is_err());
    }

    #[test]
    fn definitions_must_match_and_known_constants_must_have_correct_types() {
        let mut config = timelord_core::Config {
            node: node(),
            ..Default::default()
        };
        config.node.network_definition.as_mut().unwrap().network_id = "different".into();
        assert!(config.validate().is_err());
        config.node = node();
        config
            .node
            .network_definition
            .as_mut()
            .unwrap()
            .constants
            .insert("DIFFICULTY_STARTING".into(), "250".to_owned().into());
        assert!(resolve(&config.node).is_err());
        config.node.network_definition = None;
        assert!(config.validate().is_err());
        assert!("bad\"network".parse::<Network>().is_err());
    }

    #[test]
    fn null_constant_values_are_rejected_wherever_they_appear() {
        for value in ["null", r#"{"A":null}"#, r#"{"A":[1,null]}"#] {
            let text = format!(
                r#"{{"schema_version":1,"network_id":"ephemeral-fixture","exported_at":"f",
                    "chia_version":"f","constants":{value}}}"#
            );
            assert!(
                serde_json::from_str::<timelord_core::NetworkDefinition>(&text).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn full_u128_constants_survive_json_toml_and_helper_json() {
        for expected in [1u128 << 67, (1u128 << 127) + 1, u128::MAX] {
            let mut json = serde_json::to_value(node()).unwrap();
            // Simulate raw numeric JSON produced by the exporter, not tagged config.
            json["network_definition"]["constants"]["DIFFICULTY_CONSTANT_FACTOR"] =
                serde_json::from_str(&expected.to_string()).unwrap();
            let config = timelord_core::Config {
                node: serde_json::from_value(json).unwrap(),
                ..Default::default()
            };
            let restored: timelord_core::Config =
                toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            let restored: timelord_core::Config =
                serde_json::from_str(&serde_json::to_string(&restored).unwrap()).unwrap();
            assert_eq!(
                resolve(&restored.node)
                    .unwrap()
                    .constants
                    .difficulty_constant_factor,
                expected
            );
        }
        let yaml = yaml_rust2::YamlLoader::load_from_str(
            "DIFFICULTY_CONSTANT_FACTOR: 147573952589676412928",
        )
        .unwrap();
        assert_eq!(
            yaml_overrides(&yaml[0]).unwrap()["DIFFICULTY_CONSTANT_FACTOR"]
                .unsigned::<u128>()
                .unwrap(),
            1u128 << 67
        );
        let null = yaml_rust2::YamlLoader::load_from_str("SOMETHING: null").unwrap();
        assert!(yaml_overrides(&null[0]).is_err());
    }

    #[test]
    fn invalid_scheduler_parameters_cannot_replace_a_saved_config() {
        let directory =
            std::env::temp_dir().join(format!("tl-invalid-network-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        let mut config = timelord_core::Config {
            node: node(),
            ..Default::default()
        };
        crate::save(&path, &config).unwrap();
        let saved = std::fs::read(&path).unwrap();
        for (name, value) in [
            ("NUM_SPS_SUB_SLOT", 0),
            ("DIFFICULTY_STARTING", 0),
            ("SUB_SLOT_ITERS_STARTING", 0),
            ("DISCRIMINANT_SIZE_BITS", 17),
        ] {
            config.node = node();
            config
                .node
                .network_definition
                .as_mut()
                .unwrap()
                .constants
                .insert(name.into(), value.into());
            assert!(crate::save(&path, &config).is_err(), "{name}");
            assert_eq!(std::fs::read(&path).unwrap(), saved);
            let edited = directory.join("invalid.toml");
            std::fs::write(&edited, toml::to_string(&config).unwrap()).unwrap();
            assert!(crate::load(&edited).is_err(), "{name}");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
