//! Resolve Chia network definitions without changing the pinned scheduler.
use anyhow::{Context, Result, bail};
use dg_xch_core::consensus::{
    chain_definition::{ChainSelection, ResolvedChain},
    constants::MAINNET,
};
use timelord_core::{Network, NodeConfig};

pub fn handshake_id(node: &NodeConfig) -> &str {
    node.network.label()
}

pub fn resolve(node: &NodeConfig) -> Result<ResolvedChain> {
    let Network::Custom(_) = &node.network else {
        return ChainSelection::from_config(node.network.label(), None)
            .and_then(|chain| chain.resolve())
            .map_err(anyhow::Error::msg);
    };
    let definition = node
        .network_definition
        .as_ref()
        .context("Missing custom network definition")?;
    definition
        .validate(&node.network)
        .map_err(anyhow::Error::msg)?;
    let mut constants = MAINNET;
    constants.is_testnet = true;
    for (name, value) in &definition.constants {
        // These are the fields represented by the pinned upstream constants.
        // Keep newer fields in the definition; they do not extend its capabilities.
        macro_rules! numeric {
            ($($field:ident),* $(,)?) => {
                match name.to_ascii_lowercase().as_str() {
                    $(stringify!($field) => {
                        constants.$field = serde_json::from_value(value.clone())
                            .with_context(|| format!("Invalid network constant {name}"))?;
                        continue;
                    })*
                    _ => {}
                }
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
            min_plot_strength,
            max_plot_strength,
            plot_v1_phase_out_epoch_bits,
            plot_filter_v2_first_adjustment_height,
            plot_filter_v2_second_adjustment_height,
            plot_filter_v2_third_adjustment_height,
        );
        match name.as_str() {
            "GENESIS_CHALLENGE" => constants.genesis_challenge = hex_constant(name, value)?,
            "AGG_SIG_ME_ADDITIONAL_DATA" => {
                constants.agg_sig_me_additional_data = hex_constant(name, value)?
            }
            "GENESIS_PRE_FARM_POOL_PUZZLE_HASH" => {
                constants.genesis_pre_farm_pool_puzzle_hash = hex_constant(name, value)?
            }
            "GENESIS_PRE_FARM_FARMER_PUZZLE_HASH" => {
                constants.genesis_pre_farm_farmer_puzzle_hash = hex_constant(name, value)?
            }
            "MIN_PLOT_SIZE_V1" => constants.min_plot_size = serde_json::from_value(value.clone())?,
            "MAX_PLOT_SIZE_V1" => constants.max_plot_size = serde_json::from_value(value.clone())?,
            "NUMBER_ZERO_BITS_PLOT_FILTER_V1" => {
                constants.number_zero_bits_plot_filter = serde_json::from_value(value.clone())?
            }
            "NETWORK_TYPE" => match value.as_u64() {
                Some(0) => constants.is_testnet = false,
                Some(1) => constants.is_testnet = true,
                _ => bail!("Invalid NETWORK_TYPE"),
            },
            _ => {}
        }
    }
    // Custom Chia networks are not the upstream DGX chain format: their handshake
    // ID is the exact selected_network, with no genesis suffix or derived seed.
    Ok(ResolvedChain {
        constants,
        network_id: node.network.label().into(),
        handshake_network_id: node.network.label().into(),
        allows_bootstrap: false,
    })
}

fn hex_constant(
    name: &str,
    value: &serde_json::Value,
) -> Result<dg_xch_core::blockchain::sized_bytes::Bytes32> {
    use std::str::FromStr;
    let text = value.as_str().context("Expected hex network constant")?;
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Invalid 32-byte hex network constant {name}");
    }
    dg_xch_core::blockchain::sized_bytes::Bytes32::from_str(text).map_err(anyhow::Error::msg)
}

pub fn yaml_overrides(
    value: &yaml_rust2::Yaml,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>> {
    fn json(value: &yaml_rust2::Yaml) -> Result<serde_json::Value> {
        use yaml_rust2::Yaml;
        Ok(match value {
            Yaml::String(v) => v.clone().into(),
            Yaml::Integer(v) => (*v).into(),
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
            _ => bail!("Unsupported network override value"),
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
        assert_eq!(resolved.handshake_network_id, "ephemeral-fixture");
        assert_eq!(handshake_id(&node), "ephemeral-fixture");
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
        assert!(!resolved.allows_bootstrap);
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
            .insert("DIFFICULTY_STARTING".into(), "250".into());
        assert!(resolve(&config.node).is_err());
        config.node.network_definition = None;
        assert!(config.validate().is_err());
        assert!("bad\"network".parse::<Network>().is_err());
    }
}
