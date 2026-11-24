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
fn real_exported_override_sets_import_across_chia_versions() {
    // Exporter-shaped network.json files whose constants are the verbatim
    // `network_overrides.constants[<network>]` blocks from these releases.
    let fixtures = [
        (
            include_str!("../../../../fixtures/networks/chia-2.1-testnet10.json"),
            "testnet10",
            false,
            false,
        ),
        (
            include_str!("../../../../fixtures/networks/chia-2.3-testnet10.json"),
            "testnet10",
            true,
            false,
        ),
        (
            include_str!("../../../../fixtures/networks/chia-2.4-testnet11-fork.json"),
            "testnet11-fork",
            true,
            true,
        ),
        (
            include_str!("../../../../fixtures/networks/chia-current-testneta.json"),
            "testneta",
            false,
            false,
        ),
    ];
    for (text, network, soft_fork4, soft_fork5) in fixtures {
        let definition: timelord_core::NetworkDefinition = serde_json::from_str(text).unwrap();
        assert_eq!(definition.network_id, network);
        assert_eq!(
            definition.constants.contains_key("SOFT_FORK4_HEIGHT"),
            soft_fork4,
            "{network}"
        );
        assert_eq!(
            definition.constants.contains_key("SOFT_FORK5_HEIGHT"),
            soft_fork5,
            "{network}"
        );
        let node = NodeConfig {
            network: network.parse().unwrap(),
            network_definition: Some(definition),
            ..Default::default()
        };
        let resolved = resolve(&node).unwrap();
        assert_eq!(resolved.handshake_network_id, network);
    }
}

#[test]
fn historical_chia_overrides_are_typed_and_accepted() {
    let mut node = node();
    for (name, value) in [
        ("SOFT_FORK4_HEIGHT", 1),
        ("SOFT_FORK5_HEIGHT", 2),
        ("SOFT_FORK6_HEIGHT", 3),
        ("SOFT_FORK7_HEIGHT", 4),
        ("INITIAL_FREEZE_PERIOD", 100),
        ("INITIAL_FREEZE_END_TIMESTAMP", 1_618_776_900),
    ] {
        node.network_definition
            .as_mut()
            .unwrap()
            .constants
            .insert(name.into(), value.into());
    }
    resolve(&node).unwrap();
    node.network_definition
        .as_mut()
        .unwrap()
        .constants
        .insert("SOFT_FORK4_HEIGHT".into(), "not-a-height".to_owned().into());
    assert!(resolve(&node).is_err());
}

#[test]
fn unused_chia_keys_still_check_their_types() {
    for (name, value) in [
        ("BECH32M_PREFIX", "".to_owned().into()),
        (
            "AGG_SIG_PARENT_ADDITIONAL_DATA",
            "not hex".to_owned().into(),
        ),
    ] {
        let mut node = node();
        node.network_definition
            .as_mut()
            .unwrap()
            .constants
            .insert(name.into(), value);
        assert!(resolve(&node).is_err(), "{name}");
    }
    let mut node = node();
    node.network_definition
        .as_mut()
        .unwrap()
        .constants
        .insert("BECH32M_PREFIX".into(), "xch".to_owned().into());
    resolve(&node).unwrap();
}

#[test]
fn network_type_flag_rejects_values_other_than_zero_or_one() {
    for value in [2, -1] {
        for key in ["TESTNET", "NETWORK_TYPE"] {
            let mut node = node();
            let constants = &mut node.network_definition.as_mut().unwrap().constants;
            constants.remove("NETWORK_TYPE");
            constants.insert(key.into(), value.into());
            assert!(resolve(&node).is_err(), "{key} {value}");
        }
    }
}

#[test]
fn pasted_default_constants_import_cleanly() {
    // Some operators paste Chia's full DEFAULT_CONSTANTS into
    // `network_overrides.constants`; that remains importable too.
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
    let yaml =
        yaml_rust2::YamlLoader::load_from_str("DIFFICULTY_CONSTANT_FACTOR: 147573952589676412928")
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
    let directory = std::env::temp_dir().join(format!("tl-invalid-network-{}", std::process::id()));
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
