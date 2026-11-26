use super::*;
use crate::hardware::native::{ClientLog, NativeOwner, startup};
use std::process::Stdio;

#[tokio::test]
async fn owners_reuse_the_validated_capabilities_and_running_binary_identity() {
    let directory = std::env::temp_dir().join(format!(
        "tl-native-facts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
root=$(dirname "$0")
case "$1" in
 --control-version) echo version >> "$root/probes"; echo timelord-live-control-v1; exit 0;;
 --control-frequencies) echo frequencies >> "$root/probes"; echo '[1450,1460]'; exit 0;;
esac
echo running > "$root/running"
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
"#,
    );
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary.clone());
    config.hardware.boards = (0..3).map(|index| toml::from_str(&format!(
        "id='b{index}'\nname='Board {index}'\nserial='s{index}'\nclient_port={}\ndevice_location={}\nprofile='automatic'",18110 + index,index + 1)).unwrap()).collect();
    let native = prepare(&config).await.unwrap();
    let shared = crate::Shared::new(
        native.clone(),
        config.clone(),
        std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
            false,
        ))),
        crate::operational::State::fixture(),
        crate::unix_now(),
        crate::unix_now(),
    )
    .routed_by_config();
    let mut command = Command::new(&binary);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !directory.join("running").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // This live child already owns the validated binary; creating its control
    // owner must not probe or fingerprint the pathname again.
    std::fs::remove_file(&binary).unwrap();
    let mut owner = NativeOwner::attach(&config.hardware.boards[0], &mut child).unwrap();
    let startup = startup(&shared, &config.hardware.boards[0]).await.unwrap();
    let mut log = ClientLog::new(child.stderr.take().unwrap());
    owner
        .initialize(&shared, &config.hardware.boards[0], &mut log, &startup)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.join("probes")).unwrap(),
        "version\nfrequencies\n"
    );
    drop(owner);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn prepare_rejects_unsupported_automatic_start_frequency() {
    let directory = std::env::temp_dir().join(format!(
        "tl-prepare-frequency-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
case "$1" in
 --control-version) echo timelord-live-control-v1; exit 0;;
 --control-frequencies) echo '[1450]'; exit 0;;
esac
"#,
    );
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary);
    config.hardware.boards.push(
        toml::from_str(
            "id='b0'\nname='Board 0'\nserial='s0'\nclient_port=18110\ndevice_location=1\nprofile='automatic'",
        )
        .unwrap(),
    );
    config.hardware.boards[0].automatic.start_frequency_mhz = 1455.;
    let error = prepare(&config)
        .await
        .err()
        .expect("unsupported Automatic start frequency was accepted");
    assert!(error.to_string().contains("not supported"), "{error}");
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn prepare_skips_capabilities_when_the_board_list_is_empty() {
    let mut config = Config::default();
    config.hardware.client_binary = Some("/not-present/client".into());
    assert!(config.hardware.boards.is_empty());
    assert!(prepare(&config).await.unwrap().is_none());
    assert!(prepared_native(&config, None).await.unwrap().is_none());
}

#[tokio::test]
async fn prepared_native_loads_capabilities_only_after_automatic_boards_appear() {
    let directory = std::env::temp_dir().join(format!(
        "tl-prepared-native-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
case "$1" in
 --control-version) echo timelord-live-control-v1; exit 0;;
 --control-frequencies) echo '[1450,1460]'; exit 0;;
esac
exit 1
"#,
    );
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary);
    assert!(prepared_native(&config, None).await.unwrap().is_none());
    config.hardware.boards = (0..3)
        .map(|index| {
            toml::from_str(&format!(
                "id='board-{index}'\nname='Board {index}'\nserial='TL-TEST-{index}'\nclient_port={}\ndevice_location={}\nprofile='automatic'",
                18110 + index,
                index + 1
            ))
            .unwrap()
        })
        .collect();
    let native = prepared_native(&config, None).await.unwrap();
    assert!(native.is_some());
    let reused = prepared_native(&config, native.clone()).await.unwrap();
    assert!(reused.is_some());
    std::fs::remove_dir_all(directory).unwrap();
}
