use super::*;
#[tokio::test]
async fn thermal_probe_is_allowed_below_optimization_minima() {
    let board: BoardConfig = toml::from_str("id='probe'\nname='Probe'\nprofile='automatic'\nclient_port=18110\n[automatic]\nmin_frequency_mhz=1600.0\nmin_voltage_v=0.9").unwrap();
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let owner = NativeOwner::attach(&board, &mut child).unwrap();
    assert!(owner.control.accepts(Point {
        frequency_mhz: 200.,
        voltage_v: 0.7
    }));
    child.kill().await.unwrap();
}
#[test]
fn recovery_records_are_parsed_once_at_the_native_boundary() {
    assert_eq!(
        Recovery::parse("2026-11-04 VDF 2: Restarting VDF at 123 iters"),
        Some(Recovery { engine: 2 })
    );
    for line in [
        "Restarting VDF at 123 iters",
        "VDF 3: Restarting VDF at 123 iters",
        "VDF 0: Restarting VDF at nope iters",
        "VDF 0: Restarting VDF at 123 iters unrelated text",
    ] {
        assert_eq!(Recovery::parse(line), None);
    }
}

#[test]
fn recovery_records_match_a_captured_hardware_client_log() {
    let log = include_str!("../../../tests/fixtures/native-recovery-stderr.txt");
    let engines: Vec<u8> = log
        .lines()
        .filter_map(Recovery::parse)
        .map(|recovery| recovery.engine)
        .collect();
    assert_eq!(engines, [0, 0, 0, 2, 0, 0, 1, 0, 0]);
}
