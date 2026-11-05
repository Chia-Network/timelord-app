use super::*;
use crate::Shared;
use crate::hardware::native::{ClientLog, NativeOwner, startup};
use std::{os::unix::fs::PermissionsExt, process::Stdio};
use tokio::sync::oneshot;

#[tokio::test]
async fn owners_reuse_the_validated_capabilities_and_running_binary_identity() {
    let directory = std::env::temp_dir().join(format!("tl-native-facts-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("client");
    std::fs::write(
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
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary.clone());
    config.hardware.boards = (0..3).map(|index| toml::from_str(&format!(
        "id='b{index}'\nname='Board {index}'\nserial='s{index}'\nclient_port={}\ndevice_location={}\nprofile='automatic'",18110 + index,index + 1)).unwrap()).collect();
    let mut prepared = prepare_for(&config, &Default::default()).await.unwrap();
    assert_eq!(prepared.assignments.len(), 3);
    let shared = Shared {
        config: config.clone(),
        assignments: prepared.assignments.clone(),
        native: prepared.native.clone(),
        clients: Default::default(),
        process: crate::operational::State::fixture(),
        scope: crate::runtime::RuntimeScope::Production,
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    };
    let mut child = Command::new(&binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
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
    let identity = prepared.native.clone().unwrap();
    let original = prepared.assignments.clone();
    prepared
        .reconcile(&config, &std::collections::HashSet::from(["b0".into()]))
        .await
        .unwrap();
    assert!(
        prepared
            .assignments
            .iter()
            .all(|assignment| assignment.board_id != "b0")
    );
    assert_eq!(prepared.assignments.len(), 3);
    assert!(Arc::ptr_eq(prepared.native.as_ref().unwrap(), &identity));
    prepared
        .reconcile(&config, &Default::default())
        .await
        .unwrap();
    assert_eq!(prepared.assignments, original);

    let mut owner = NativeOwner::attach(&config.hardware.boards[0], &mut child).unwrap();
    let startup = startup(&shared, &config.hardware.boards[0]).await.unwrap();
    let (ready, _rx) = oneshot::channel();
    let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
    owner
        .initialize(&shared, &config.hardware.boards[0], &mut log, startup)
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
async fn reconcile_resolves_assignments_without_revalidating_automatic_boards() {
    let mut config = Config::default();
    config.hardware.boards.push(
        toml::from_str(
            "id='b0'\nname='Board 0'\nserial='s0'\nclient_port=18110\ndevice_location=1\nprofile='automatic'",
        )
        .unwrap(),
    );
    config.hardware.boards[0].automatic.start_frequency_mhz = 1455.;
    let mut prepared = Prepared {
        assignments: vec![],
        native: Some(Arc::new(Facts {
            hash: "hash".into(),
            host: "host".into(),
            frequencies: vec![1450.],
        })),
    };
    let unavailable = std::collections::HashSet::from(["b0".into()]);
    prepared.reconcile(&config, &unavailable).await.unwrap();
    assert!(prepared.assignments.is_empty());
}

#[tokio::test]
async fn prepare_rejects_unsupported_automatic_start_frequency() {
    let directory =
        std::env::temp_dir().join(format!("tl-prepare-frequency-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("client");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
case "$1" in
 --control-version) echo timelord-live-control-v1; exit 0;;
 --control-frequencies) echo '[1450]'; exit 0;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary);
    config.hardware.boards.push(
        toml::from_str(
            "id='b0'\nname='Board 0'\nserial='s0'\nclient_port=18110\ndevice_location=1\nprofile='automatic'",
        )
        .unwrap(),
    );
    config.hardware.boards[0].automatic.start_frequency_mhz = 1455.;
    let error = prepare_for(&config, &Default::default())
        .await
        .err()
        .expect("unsupported Automatic start frequency was accepted");
    assert!(error.to_string().contains("not supported"), "{error}");
    std::fs::remove_dir_all(directory).unwrap();
}
