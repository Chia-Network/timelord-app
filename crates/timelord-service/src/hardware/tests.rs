use super::*;

#[test]
fn capability_accepts_the_complete_2200_mhz_grid_and_rejects_invalid_lists() {
    let full: Vec<f64> = (200..=2200).step_by(10).map(f64::from).collect();
    assert_eq!(
        super::decode_frequencies(&serde_json::to_vec(&full).unwrap()).unwrap(),
        full
    );
    for invalid in [
        vec![],
        vec![200., 2201.],
        vec![200., 200.],
        vec![2200., 200.],
        vec![200.; 202],
    ] {
        assert!(super::decode_frequencies(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
}

#[test]
fn automatic_and_manual_arguments_share_the_optimized_proof_settings() {
    let mut board: BoardConfig =
        toml::from_str("id='a'\nname='A'\nclient_port=18110\ndevice_location=1").unwrap();
    let automatic = arguments(&board, 3).unwrap();
    assert!(automatic.contains(&"--live-control".into()));
    assert!(automatic.contains(&"--retain-asic-values".into()));
    assert!(automatic.windows(2).any(|w| w == ["--freq", "1450"]));
    board.profile = ProfileMode::Manual;
    board.frequency_mhz = 1540.;
    let manual = arguments(&board, 1).unwrap();
    assert!(!manual.contains(&"--live-control".into()));
    assert!(manual.contains(&"--retain-asic-values".into()));
    assert!(manual.windows(2).any(|w| w == ["--freq", "1540"]));
    assert!(
        manual
            .windows(2)
            .any(|w| w == ["--checkpoint-threshold", "250000"])
    );
}

#[test]
fn native_mask_matches_physical_assignments_and_profile_mode() {
    let mut config = Config::default();
    for boards in 1..=3 {
        config.hardware.boards = (0..boards)
            .map(|n| {
                toml::from_str(&format!(
                    "id='b{n}'\nname='Board {n}'\nclient_port={}\ndevice_location={}\n",
                    18110 + n,
                    n + 1
                ))
                .unwrap()
            })
            .collect();
        let assignments = config.assignments().unwrap();
        for board in &config.hardware.boards {
            let assigned: Vec<_> = assignments
                .iter()
                .filter(|a| a.board_id == board.id)
                .collect();
            let expected = assigned.iter().fold(0u8, |mask, a| mask | (1 << a.engine));
            for mode in [ProfileMode::Automatic, ProfileMode::Manual] {
                let mut board = board.clone();
                board.profile = mode;
                let args = arguments(&board, assigned.len()).unwrap();
                assert!(
                    args.windows(2)
                        .any(|w| w == ["--vdfs-mask", &expected.to_string()])
                );
            }
        }
    }
    let board = &config.hardware.boards[0];
    assert!(arguments(board, 0).is_err());
    assert!(arguments(board, 4).is_err());
}
