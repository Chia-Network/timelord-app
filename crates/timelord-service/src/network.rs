//! Resolve Chia network definitions without changing the pinned scheduler.
use anyhow::{Context, Result, bail};
use dg_xch_core::consensus::{
    chain_definition::{ChainSelection, ResolvedChain},
    constants::{ConsensusConstants, MAINNET},
};
use std::collections::BTreeMap;
use timelord_core::{Network, NetworkValue, NodeConfig};

/// Chia override/consensus keys the pinned scheduler does not consume. The
/// exporter copies `network_overrides.constants[<network>]` verbatim, Chia never
/// prunes stale keys, and its own loader ignores unknown names, so real exports
/// across releases carry these. Accept them with their types checked; never
/// silently ignore arbitrary names.
const UNUSED_CHIA_CONSTANTS: &[&str] = &[
    "AGG_SIG_PARENT_ADDITIONAL_DATA",
    "AGG_SIG_PUZZLE_ADDITIONAL_DATA",
    "AGG_SIG_AMOUNT_ADDITIONAL_DATA",
    "AGG_SIG_PUZZLE_AMOUNT_ADDITIONAL_DATA",
    "AGG_SIG_PARENT_AMOUNT_ADDITIONAL_DATA",
    "AGG_SIG_PARENT_PUZZLE_ADDITIONAL_DATA",
    "BECH32M_PREFIX",
    // Chia 2.3-2.5 shipped these forks; the pinned scheduler has no field yet.
    "SOFT_FORK4_HEIGHT",
    "SOFT_FORK5_HEIGHT",
    "SOFT_FORK6_HEIGHT",
    "SOFT_FORK7_HEIGHT",
    // Early testnet overrides that current Chia also ignores.
    "INITIAL_FREEZE_PERIOD",
    "INITIAL_FREEZE_END_TIMESTAMP",
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
    match name {
        "BECH32M_PREFIX" => {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .context("BECH32M_PREFIX must be a nonempty string")?;
        }
        name if name.starts_with("AGG_SIG_") => {
            let text = value.as_str().context("Expected a 32-byte hex value")?;
            if !timelord_core::is_hex_32(text) {
                bail!("Network constant {name} must be 32 bytes of hex");
            }
        }
        _ => {
            value
                .unsigned::<u64>()
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("Invalid network constant {name}"))?;
        }
    }
    Ok(())
}

fn set(
    constants: &mut ConsensusConstants,
    name: &str,
    source: &str,
    value: &NetworkValue,
) -> Result<()> {
    // Keys are already validated as uppercase, so this is an exact-name table.
    macro_rules! numeric {
        ($(($name:literal, $field:ident)),* $(,)?) => {
            match name {
                $(
                    $name => {
                        constants.$field = value.unsigned().map_err(anyhow::Error::msg)
                            .with_context(|| format!("Invalid network constant {source}"))?;
                        return Ok(());
                    }
                )*
                _ => {}
            }
        };
    }
    numeric!(
        ("SLOT_BLOCKS_TARGET", slot_blocks_target),
        (
            "MIN_BLOCKS_PER_CHALLENGE_BLOCK",
            min_blocks_per_challenge_block
        ),
        ("MAX_SUB_SLOT_BLOCKS", max_sub_slot_blocks),
        ("NUM_SPS_SUB_SLOT", num_sps_sub_slot),
        ("SUB_SLOT_ITERS_STARTING", sub_slot_iters_starting),
        ("DIFFICULTY_CONSTANT_FACTOR", difficulty_constant_factor),
        ("DIFFICULTY_STARTING", difficulty_starting),
        ("DIFFICULTY_CHANGE_MAX_FACTOR", difficulty_change_max_factor),
        ("SUB_EPOCH_BLOCKS", sub_epoch_blocks),
        ("EPOCH_BLOCKS", epoch_blocks),
        ("SIGNIFICANT_BITS", significant_bits),
        ("DISCRIMINANT_SIZE_BITS", discriminant_size_bits),
        ("NUMBER_ZERO_BITS_PLOT_FILTER", number_zero_bits_plot_filter),
        ("MIN_PLOT_SIZE", min_plot_size),
        ("MAX_PLOT_SIZE", max_plot_size),
        ("SUB_SLOT_TIME_TARGET", sub_slot_time_target),
        ("NUM_SP_INTERVALS_EXTRA", num_sp_intervals_extra),
        ("MAX_FUTURE_TIME", max_future_time),
        ("MAX_FUTURE_TIME2", max_future_time2),
        ("NUMBER_OF_TIMESTAMPS", number_of_timestamps),
        ("MAX_VDF_WITNESS_SIZE", max_vdf_witness_size),
        ("MEMPOOL_BLOCK_BUFFER", mempool_block_buffer),
        ("MAX_COIN_AMOUNT", max_coin_amount),
        ("MAX_BLOCK_COST_CLVM", max_block_cost_clvm),
        ("COST_PER_BYTE", cost_per_byte),
        ("WEIGHT_PROOF_THRESHOLD", weight_proof_threshold),
        ("WEIGHT_PROOF_RECENT_BLOCKS", weight_proof_recent_blocks),
        ("MAX_BLOCK_COUNT_PER_REQUESTS", max_block_count_per_requests),
        ("BLOCKS_CACHE_SIZE", blocks_cache_size),
        ("MAX_GENERATOR_SIZE", max_generator_size),
        ("MAX_GENERATOR_REF_LIST_SIZE", max_generator_ref_list_size),
        ("POOL_SUB_SLOT_ITERS", pool_sub_slot_iters),
        ("SOFT_FORK2_HEIGHT", soft_fork2_height),
        ("SOFT_FORK3_HEIGHT", soft_fork3_height),
        ("SOFT_FORK8_HEIGHT", soft_fork8_height),
        ("SOFT_FORK9_HEIGHT", soft_fork9_height),
        ("HARD_FORK_HEIGHT", hard_fork_height),
        ("HARD_FORK_FIX_HEIGHT", hard_fork_fix_height),
        ("HARD_FORK2_HEIGHT", hard_fork2_height),
        ("PLOT_FILTER_128_HEIGHT", plot_filter_128_height),
        ("PLOT_FILTER_64_HEIGHT", plot_filter_64_height),
        ("PLOT_FILTER_32_HEIGHT", plot_filter_32_height),
        (
            "NUMBER_ZERO_BITS_PLOT_FILTER_V2",
            number_zero_bits_plot_filter_v2
        ),
        ("PLOT_SIZE_V2", plot_size_v2),
        ("FILTER_WINDOW_SIZE", filter_window_size),
        (
            "MAX_EFFECTIVE_PLOT_FILTER_BITS",
            max_effective_plot_filter_bits
        ),
        ("MIN_PLOT_STRENGTH", min_plot_strength),
        ("MAX_PLOT_STRENGTH", max_plot_strength),
        ("PLOT_V1_PHASE_OUT_EPOCH_BITS", plot_v1_phase_out_epoch_bits),
        (
            "PLOT_FILTER_V2_FIRST_ADJUSTMENT_HEIGHT",
            plot_filter_v2_first_adjustment_height
        ),
        (
            "PLOT_FILTER_V2_SECOND_ADJUSTMENT_HEIGHT",
            plot_filter_v2_second_adjustment_height
        ),
        (
            "PLOT_FILTER_V2_THIRD_ADJUSTMENT_HEIGHT",
            plot_filter_v2_third_adjustment_height
        ),
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
        _ if UNUSED_CHIA_CONSTANTS.contains(&name) => unused_constant(name, value)?,
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
pub(crate) mod tests;
