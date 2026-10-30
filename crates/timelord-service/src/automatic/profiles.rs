//! Learned operating points, machine identity and on-disk schema migration.
use super::{Point, RetestState};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use timelord_core::Config;

pub fn fingerprint(config: &Config) -> Result<String> {
    let bytes = std::fs::read(
        config
            .hardware
            .client_binary
            .as_ref()
            .context("Choose a native client")?,
    )?;
    Ok(dg_xch_core::utils::hash_256(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
pub fn host() -> String {
    format!(
        "{}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.model"])
            .output()
            .ok()
            .map(|r| String::from_utf8_lossy(&r.stdout).trim().to_string())
            .unwrap_or_default()
    )
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Learned {
    pub serial: String,
    pub engines: usize,
    // Missing in older profiles, which used the lowest-numbered engines.
    #[serde(default)]
    pub engine_mask: Option<u8>,
    pub host: String,
    pub client_sha256: String,
    pub point: Point,
    pub active_seconds: u64,
    pub updated_unix: u64,
    pub ceiling_mhz: f64,
    pub cooldown_seconds: u64,
    #[serde(default)]
    pub performance: Option<crate::assignment::Performance>,
    #[serde(default)]
    pub retest: Option<RetestState>,
}
impl Learned {
    fn physical_mask(&self) -> Option<u8> {
        self.engine_mask.or_else(|| match self.engines {
            1..=3 => Some((1 << self.engines) - 1),
            _ => None,
        })
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    schema_version: u32,
    entries: Vec<Learned>,
}
fn cache(path: &Path) -> Result<Cache> {
    if !path.exists() {
        return Ok(Cache {
            schema_version: 2,
            ..Default::default()
        });
    }
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        bail!("Invalid Automatic state file");
    }
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if value["schema_version"] == 1 {
        // Preserve observed operating points while discarding zero-recovery
        // ceilings and obsolete voltage-search state from the previous policy.
        if let Some(entries) = value["entries"].as_array_mut() {
            for entry in entries {
                if let Some(entry) = entry.as_object_mut() {
                    entry.remove("voltage_rejected");
                    entry.insert("ceiling_mhz".into(), 1600.into());
                    entry.insert("cooldown_seconds".into(), 0.into());
                }
            }
        }
        value["schema_version"] = 2.into();
    }
    let value: Cache = serde_json::from_value(value)?;
    if value.schema_version != 2 {
        bail!("Unsupported Automatic state version");
    }
    Ok(value)
}
pub fn learned(
    config: &Config,
    board: &timelord_core::BoardConfig,
    engines: usize,
    hash: &str,
    host: &str,
) -> Result<Option<Learned>> {
    let Some(path) = config.hardware.automatic_state.as_ref() else {
        return Ok(None);
    };
    Ok(cache(path)?
        .entries
        .into_iter()
        .filter(|e| {
            Some(&e.serial) == board.serial.as_ref()
                && e.engines == engines
                && e.physical_mask() == timelord_core::engine_mask(engines)
                && e.client_sha256 == hash
                && e.host == host
                && board
                    .automatic
                    .contains(e.point.frequency_mhz, e.point.voltage_v)
        })
        .max_by_key(|e| e.updated_unix))
}
pub fn initial(
    config: &Config,
    board: &timelord_core::BoardConfig,
    engines: usize,
    hash: &str,
    host: &str,
) -> Result<Point> {
    Ok(learned(config, board, engines, hash, host)?
        .map(|e| Point {
            voltage_v: e.point.voltage_v.max(board.automatic.start_voltage_v),
            ..e.point
        })
        .unwrap_or(Point {
            frequency_mhz: board.automatic.start_frequency_mhz,
            voltage_v: board.automatic.start_voltage_v,
        }))
}
pub fn save(path: &Path, entry: Learned) -> Result<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("Automatic state lock failed"))?;
    let mut value = cache(path)?;
    value.entries.retain(|e| {
        !(e.serial == entry.serial
            && e.engines == entry.engines
            && e.physical_mask() == entry.physical_mask()
            && e.host == entry.host
            && e.client_sha256 == entry.client_sha256)
    });
    value.entries.push(entry);
    if value.entries.len() > 128 {
        value.entries.drain(..value.entries.len() - 128);
    }
    let parent = path.parent().context("Automatic state folder missing")?;
    std::fs::create_dir_all(parent)?;
    timelord_supervisor::atomic::write(path, &serde_json::to_vec_pretty(&value)?)
}

pub fn socket_path(config_path: &Path, config: &Config) -> PathBuf {
    config.service.control_socket.clone().unwrap_or_else(|| {
        let canonical = config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.to_owned());
        let digest = dg_xch_core::utils::hash_256(canonical.as_os_str().as_encoded_bytes());
        let suffix: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        // macOS limits Unix socket paths to 104 bytes. Long Chia/config roots
        // therefore use a stable, same-user private temporary socket path.
        std::env::temp_dir().join(format!("tl-{}-{suffix}.sock", unsafe { libc::geteuid() }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automatic::Policy;
    #[test]
    fn old_profiles_keep_the_observed_frequency_without_old_recovery_or_voltage_search_limits() {
        let path =
            std::env::temp_dir().join(format!("tl-profile-migration-{}.json", std::process::id()));
        let old = serde_json::json!({"schema_version":1,"entries":[{
            "serial":"a","engines":2,"host":"mac","client_sha256":"hash",
            "point":{"frequency_mhz":1500.,"voltage_v":0.95},"active_seconds":120,
            "updated_unix":1,"ceiling_mhz":1500.,"cooldown_seconds":900,"voltage_rejected":true}]});
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let mut config = Config::default();
        config.hardware.automatic_state = Some(path.clone());
        let board: timelord_core::BoardConfig =
            toml::from_str("id='a'\nname='A'\nclient_port=18110\nserial='a'").unwrap();
        let point = initial(&config, &board, 2, "hash", "mac").unwrap();
        assert_eq!(
            point,
            Point {
                frequency_mhz: 1500.,
                voltage_v: 1.
            }
        );
        let migrated = cache(&path).unwrap();
        assert_eq!(migrated.schema_version, 2);
        assert_eq!(migrated.entries[0].ceiling_mhz, 1600.);
        assert_eq!(migrated.entries[0].cooldown_seconds, 0);
        let mut entry = migrated.entries[0].clone();
        entry.point = point;
        save(&path, entry).unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("voltage_rejected"));
        assert_eq!(cache(&path).unwrap().entries.len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn simultaneous_board_updates_keep_all_learned_profiles() {
        let path =
            std::env::temp_dir().join(format!("tl-cache-concurrent-{}.json", std::process::id()));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    save(
                        &path,
                        Learned {
                            serial: format!("board-{n}"),
                            engines: 1,
                            engine_mask: Some(2),
                            host: "mac".into(),
                            client_sha256: "hash".into(),
                            point: Point {
                                frequency_mhz: 1450.,
                                voltage_v: 1.,
                            },
                            active_seconds: 120,
                            updated_unix: n,
                            ceiling_mhz: 1600.,
                            cooldown_seconds: 0,
                            performance: None,
                            retest: None,
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(cache(&path).unwrap().entries.len(), 8);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cache_is_bound_to_serial_engine_host_and_binary() {
        let directory = std::env::temp_dir().join(format!(
            "automatic-cache-{}-{}",
            std::process::id(),
            crate::unix_now()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.json");
        let mut config = Config::default();
        config.hardware.automatic_state = Some(path.clone());
        let board: timelord_core::BoardConfig =
            toml::from_str("id='a'\nname='A'\nclient_port=18110\nserial='a'\nprofile='automatic'")
                .unwrap();
        let entry = Learned {
            serial: "a".into(),
            engines: 3,
            engine_mask: None,
            host: "mac".into(),
            client_sha256: "hash".into(),
            point: Point {
                frequency_mhz: 1520.,
                voltage_v: 1.,
            },
            active_seconds: 120,
            updated_unix: 1,
            ceiling_mhz: 1520.,
            cooldown_seconds: 30,
            performance: None,
            retest: None,
        };
        save(&path, entry.clone()).unwrap();
        assert_eq!(
            initial(&config, &board, 3, "hash", "mac")
                .unwrap()
                .frequency_mhz,
            1520.
        );
        let entry = learned(&config, &board, 3, "hash", "mac").unwrap().unwrap();
        let mut restored = Policy::new(
            board.automatic.clone(),
            entry.point,
            0,
            (20..=160).map(|i| f64::from(i) * 10.).collect(),
        );
        restored.restore(&entry);
        assert_eq!(restored.learned_limits(), (1520., 30));
        for (engines, hash, host) in [
            (1, "hash", "mac"),
            (3, "different", "mac"),
            (3, "hash", "other"),
        ] {
            assert_eq!(
                initial(&config, &board, engines, hash, host)
                    .unwrap()
                    .frequency_mhz,
                1450.
            );
        }
        // An old single-engine profile belongs to engine 0, not the new engine 1.
        let mut old = entry;
        old.engines = 1;
        old.engine_mask = None;
        save(&path, old.clone()).unwrap();
        assert!(
            learned(&config, &board, 1, "hash", "mac")
                .unwrap()
                .is_none()
        );
        let mut current = old;
        current.engine_mask = Some(2);
        current.ceiling_mhz = 1980.;
        save(&path, current.clone()).unwrap();
        let restored = learned(&config, &board, 1, "hash", "mac").unwrap().unwrap();
        assert_eq!(restored.engine_mask, Some(2));
        assert_eq!(restored.ceiling_mhz, 1980.);
        current.ceiling_mhz = 1960.;
        save(&path, current).unwrap();
        assert_eq!(cache(&path).unwrap().entries.len(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
