//! Cover the installer-facing launchd-values command without USB or a node.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "timelord-launchd-values-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn launchd_values(config: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_timelord-service"))
        .args(["launchd-values", "--config"])
        .arg(config)
        .output()
        .unwrap()
}

fn parsed(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn launchd_values_reads_a_profileless_node_network() {
    let directory = Directory::new();
    let value = parsed(&launchd_values(
        &directory.write("config.toml", "schema_version = 1\n"),
    ));
    assert_eq!(value["network"], "mainnet");
    assert_eq!(value["supervisor_observability_listen"], "127.0.0.1:9259");
    assert_eq!(value["status_listen"], "127.0.0.1:9257");
}

#[test]
fn launchd_values_uses_the_active_profile_network() {
    let directory = Directory::new();
    let value = parsed(&launchd_values(&directory.write(
        "config.toml",
        r#"
schema_version = 1
[connections]
active_id = "testnet"
[[connections.profiles]]
id = "mainnet"
name = "Mainnet"
node = { host = "127.0.0.1", port = 8444, network = "mainnet" }
[[connections.profiles]]
id = "testnet"
name = "Testnet11"
node = { host = "127.0.0.1", port = 58444, network = "testnet11" }
"#,
    )));
    assert_eq!(value["network"], "testnet11");
}

#[test]
fn launchd_values_migrates_a_credentialed_single_node() {
    let directory = Directory::new();
    let value = parsed(&launchd_values(&directory.write(
        "config.toml",
        r#"
schema_version = 1
[node]
host = "192.0.2.10"
port = 8444
network = "testnet11"
certificate = "client.crt"
private_key = "client.key"
ca_certificate = "ca.crt"
"#,
    )));
    assert_eq!(value["network"], "testnet11");
}

#[test]
fn launchd_values_rejects_profiles_without_a_selection() {
    let directory = Directory::new();
    let output = launchd_values(&directory.write(
        "config.toml",
        r#"
schema_version = 1
[[connections.profiles]]
id = "testnet"
name = "Testnet11"
node = { host = "127.0.0.1", port = 58444, network = "testnet11" }
"#,
    ));
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Connection profiles require one selected profile")
    );
    assert!(output.stdout.is_empty());
}
