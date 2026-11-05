use super::*;
use crate::{Shared, hardware::tuning::AutomaticOwner};
use std::{os::unix::fs::PermissionsExt, process::Stdio};

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
    let prepared = prepare(&config).await.unwrap();
    assert_eq!(prepared.assignments.len(), 3);
    let shared = Shared {
        config: config.clone(),
        assignments: prepared.assignments,
        native: prepared.native,
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    };
    let mut child = Command::new(&binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
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
    let owner = AutomaticOwner::new(&shared, &config.hardware.boards[0], &mut child)
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
