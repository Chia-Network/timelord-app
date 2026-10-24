//! Read-only D2XX enumeration. Never opens a USB handle or initializes an ASIC.
use anyhow::{Context, Result, bail};
use serde::Serialize;
#[cfg(target_os = "macos")]
use std::path::Path;
use timelord_core::Config;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Device {
    pub name: String,
    pub location: u32,
    pub serial: Option<String>,
}

#[cfg(target_os = "macos")]
fn native(path: &Path) -> Result<Vec<Device>> {
    use std::ffi::{CString, c_char, c_void};
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Node {
        flags: u32,
        kind: u32,
        id: u32,
        location: u32,
        serial: [c_char; 16],
        description: [c_char; 64],
        handle: *mut c_void,
    }
    struct Library(*mut c_void);
    impl Drop for Library {
        fn drop(&mut self) {
            unsafe {
                if !self.0.is_null() {
                    libc::dlclose(self.0);
                }
            }
        }
    }
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    // D2XX ABI from the distributed macOS ftd2xx.h/WinTypes.h: DWORD/ULONG
    // are unsigned 32-bit, with a pointer-sized handle. Only enumeration exports
    // are resolved; neither FT_Open nor any control/write export is called.
    unsafe {
        let library = Library(libc::dlopen(
            path.as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        ));
        if library.0.is_null() {
            bail!("Could not load the native client's D2XX library");
        }
        let create = libc::dlsym(library.0, c"FT_CreateDeviceInfoList".as_ptr());
        let get = libc::dlsym(library.0, c"FT_GetDeviceInfoList".as_ptr());
        if create.is_null() || get.is_null() {
            bail!("D2XX enumeration exports are missing");
        }
        let create: unsafe extern "C" fn(*mut u32) -> u32 = std::mem::transmute(create);
        let get: unsafe extern "C" fn(*mut Node, *mut u32) -> u32 = std::mem::transmute(get);
        let mut count = 0;
        if create(&mut count) != 0 || count > 256 {
            bail!("D2XX enumeration failed");
        }
        if count == 0 {
            return Ok(vec![]);
        }
        let mut nodes = vec![std::mem::zeroed::<Node>(); count as usize];
        let capacity = count;
        if get(nodes.as_mut_ptr(), &mut count) != 0 || count > capacity {
            bail!("D2XX device list changed; retry discovery");
        }
        let string = |bytes: &[c_char]| {
            String::from_utf8_lossy(
                &bytes
                    .iter()
                    .take_while(|b| **b != 0)
                    .map(|b| *b as u8)
                    .collect::<Vec<_>>(),
            )
            .into_owned()
        };
        Ok(nodes[..count as usize]
            .iter()
            .filter_map(|n| {
                let name = string(&n.description);
                let serial = string(&n.serial);
                if ![10, 11].contains(&n.kind)
                    || !name.starts_with("Chia VDF ")
                    || !name.ends_with('A')
                    || n.location == 0
                {
                    return None;
                }
                Some(Device {
                    name,
                    location: n.location,
                    serial: (!serial.is_empty())
                        .then(|| serial.strip_suffix('A').unwrap_or(&serial).to_owned()),
                })
            })
            .collect())
    }
}

#[cfg(target_os = "macos")]
fn driver_path(config: &Config, helper: &Path) -> std::path::PathBuf {
    if let Some(path) = &config.hardware.driver_library {
        return path.clone();
    }
    // Discovery runs before the app saves its bundled client into a fresh config.
    // Resolve from this helper's bundle so copied apps work on first launch.
    config
        .hardware
        .client_binary
        .as_deref()
        .unwrap_or(helper)
        .parent()
        .unwrap_or(Path::new("."))
        .join("../Frameworks/libftd2xx.dylib")
}

pub async fn discover(config: &Config) -> Result<Vec<Device>> {
    #[cfg(target_os = "macos")]
    {
        let path = driver_path(config, &std::env::current_exe()?);
        if path.is_file() {
            return tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || native(&path)),
            )
            .await
            .context("USB enumeration timed out")??;
        }
    }
    bail!("Select the packaged D2XX library to enumerate ASIC serial identities")
}

/// Never substitute a different board at the old USB location for a known serial.
pub fn board_location(board: &timelord_core::BoardConfig, devices: &[Device]) -> Result<u32> {
    let matches: Vec<_> = devices
        .iter()
        .filter(|d| match &board.serial {
            Some(serial) => d.serial.as_ref() == Some(serial),
            None => board.device_location == Some(d.location),
        })
        .collect();
    if matches.len() != 1 {
        bail!(
            "{} is disconnected, busy, or has an ambiguous USB identity",
            board.name
        );
    }
    Ok(matches[0].location)
}

#[cfg(all(test, target_os = "macos"))]
mod driver_path_tests {
    use super::*;

    #[test]
    fn fresh_config_discovers_driver_from_relocated_app_without_saving_paths() {
        let config = Config::default();
        for bundle in [
            "/Applications/Timelord.app",
            "/Users/test/Downloads/My Timelord.app",
        ] {
            let helper = Path::new(bundle).join("Contents/Helpers/timelord-service");
            assert_eq!(
                driver_path(&config, &helper),
                Path::new(bundle).join("Contents/Helpers/../Frameworks/libftd2xx.dylib")
            );
        }
        assert!(config.hardware.client_binary.is_none());
        assert!(config.hardware.driver_library.is_none());
    }

    #[test]
    fn explicit_driver_and_external_client_locations_remain_authoritative() {
        let mut config = Config::default();
        let helper = Path::new("/Applications/Timelord.app/Contents/Helpers/timelord-service");
        config.hardware.client_binary = Some("/custom/Helpers/hw_vdf_client".into());
        assert_eq!(
            driver_path(&config, helper),
            Path::new("/custom/Helpers/../Frameworks/libftd2xx.dylib")
        );
        config.hardware.driver_library = Some("/custom/driver.dylib".into());
        assert_eq!(
            driver_path(&config, helper),
            Path::new("/custom/driver.dylib")
        );
    }
}

/// Resolve only physically enumerated boards, preserving the stored registry.
/// The returned plan never changes the on-disk configuration.
pub async fn connected_configuration(config: Config) -> Result<Config> {
    if config.hardware.client_binary.is_none() {
        return Ok(config);
    }
    let devices = discover(&config).await?;
    connected_plan(config, devices)
}

pub(crate) fn connected_plan(mut config: Config, devices: Vec<Device>) -> Result<Config> {
    // Preserve each board's identity and settings while it is unplugged. Learned
    // operating points are separately keyed by serial, engine count and mask.
    for board in &config.hardware.boards {
        config
            .hardware
            .remembered_boards
            .retain(|b| b.id != board.id);
        config.hardware.remembered_boards.push(board.clone());
    }
    let mut identities = std::collections::BTreeSet::new();
    for device in &devices {
        if !identities.insert((
            device.serial.clone(),
            if device.serial.is_some() {
                0
            } else {
                device.location
            },
        )) {
            bail!("Ambiguous ASIC identity; retry discovery");
        }
    }
    // A serial-pinned mapping remains fixed even if a board is absent. Its owner
    // retries discovery while the healthy boards keep their existing chains.
    let pinned = config.hardware.chain_assignments.is_some();
    config.hardware.boards.retain_mut(|board| {
        let found = devices.iter().find(|d| match &board.serial {
            Some(s) => d.serial.as_ref() == Some(s),
            None => board.device_location == Some(d.location),
        });
        if let Some(device) = found {
            board.device_location = Some(device.location);
            if device.serial.is_some() {
                board.serial = device.serial.clone();
            }
            true
        } else {
            pinned
        }
    });
    // Automatically include newly plugged boards with Automatic tuning.
    for device in devices {
        if config
            .hardware
            .boards
            .iter()
            .any(|b| b.device_location == Some(device.location))
        {
            continue;
        }
        if config.hardware.boards.len() == 3 {
            break;
        }
        let port = (18110..=18112)
            .find(|p| !config.hardware.boards.iter().any(|b| b.client_port == *p))
            .context("No board port available")?;
        let mut board: timelord_core::BoardConfig = toml::from_str(&format!(
            "id='usb-{}'\nname='ASIC {}'\nclient_port={port}\ndevice_location={}\nprofile='automatic'",
            device.location,
            config.hardware.boards.len() + 1,
            device.location
        ))?;
        if let Some(remembered) = config
            .hardware
            .remembered_boards
            .iter()
            .find(|b| b.serial.is_some() && b.serial == device.serial)
        {
            board = remembered.clone();
            board.client_port = port;
            board.device_location = Some(device.location);
        }
        board.serial = device.serial;
        config.hardware.boards.push(board);
    }
    config.validate().map_err(anyhow::Error::msg)?;
    Ok(config)
}

/// Debounce successful physical inventories. Enumeration failures break the
/// streak; they are never interpreted as unplugged boards.
#[derive(Default)]
pub struct TopologyChange {
    pending: Vec<(String, Option<String>, Option<u32>)>,
    confirmations: usize,
    pending_since: Option<std::time::Instant>,
}
impl TopologyChange {
    pub fn observe(&mut self, current: &Config, candidate: Result<Config>) -> Option<Config> {
        self.observe_at(current, candidate, std::time::Instant::now())
    }
    fn observe_at(
        &mut self,
        current: &Config,
        candidate: Result<Config>,
        now: std::time::Instant,
    ) -> Option<Config> {
        fn key(config: &Config) -> Vec<(String, Option<String>, Option<u32>)> {
            let mut key: Vec<_> = config
                .hardware
                .boards
                .iter()
                .map(|b| (b.id.clone(), b.serial.clone(), b.device_location))
                .collect();
            key.sort();
            key
        }
        let candidate = match candidate {
            Ok(config) => config,
            Err(error) => {
                tracing::debug!(%error, "USB inventory unavailable; retaining current topology");
                self.confirmations = 0;
                return None;
            }
        };
        let next = key(&candidate);
        if next == key(current) {
            self.confirmations = 0;
            return None;
        }
        if self.confirmations == 0 || next != self.pending {
            self.pending = next;
            self.pending_since = Some(now);
            self.confirmations = 1;
        } else {
            self.confirmations += 1;
        }
        // Three polls, five seconds apart: at least ten seconds of agreement.
        if self.confirmations < 3
            || self
                .pending_since
                .is_none_or(|start| now.duration_since(start) < std::time::Duration::from_secs(10))
        {
            return None;
        }
        self.confirmations = 0;
        Some(candidate)
    }
}

pub async fn live_configuration(shared: &crate::Shared) -> Result<Config> {
    // Explicit administrator mappings retain their fixed chain ownership.
    if shared.config.hardware.client_binary.is_none()
        || shared.config.hardware.chain_assignments.is_some()
    {
        return Ok(shared.config.clone());
    }
    let _initialization = crate::hardware::INITIALIZATION.lock().await;
    let mut devices = discover(&shared.config).await?;
    {
        // Keep the same lock order as the status handler.
        let clients = shared.clients.lock().expect("clients");
        let runtime = shared.runtime.lock().expect("runtime");
        // Open FTDI interfaces can omit metadata. Require recent hardware
        // telemetry as well as a live socket; an old connection is not presence.
        for board in &shared.config.hardware.boards {
            if runtime.boards.get(&board.id).is_some_and(|state| {
                state.state == "connected"
                    && state.temperature_unix.saturating_add(15) >= crate::unix_now()
            }) && clients.iter().any(|(id, _)| id == &board.id)
                && !devices.iter().any(|d| {
                    d.serial == board.serial && d.serial.is_some()
                        || Some(d.location) == board.device_location
                })
                && let Some(location) = runtime
                    .boards
                    .get(&board.id)
                    .and_then(|b| b.device_location)
                    .or(board.device_location)
            {
                devices.push(Device {
                    name: board.name.clone(),
                    serial: board.serial.clone(),
                    location,
                });
            }
        }
    }
    connected_plan(shared.config.clone(), devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn configured() -> Config {
        let mut config = Config::default();
        config.hardware.boards = (0..3).map(|i| toml::from_str(&format!(
            "id='b{i}'\nname='B{i}'\nserial='serial-{i}'\ndevice_location={}\nclient_port={}\nprofile='automatic'", i+1, i+18110
        )).unwrap()).collect();
        config.hardware.boards[0].automatic.start_frequency_mhz = 1460.;
        config
    }
    fn devices(indices: &[u32]) -> Vec<Device> {
        indices
            .iter()
            .map(|i| Device {
                name: "Chia VDF A".into(),
                location: i + 1,
                serial: Some(format!("serial-{i}")),
            })
            .collect()
    }
    #[test]
    fn automatic_topology_survives_three_one_zero_two_three_and_remembers_settings() {
        let original = configured();
        let mut config = original.clone();
        for (present, counts) in [
            (vec![0], vec![3]),
            (vec![], vec![]),
            (vec![0, 1], vec![2, 1]),
            (vec![0, 1, 2], vec![1, 1, 1]),
        ] {
            config = connected_plan(config, devices(&present)).unwrap();
            let plan = config.assignments().unwrap();
            assert_eq!(
                config
                    .hardware
                    .boards
                    .iter()
                    .map(|b| plan.iter().filter(|a| a.board_id == b.id).count())
                    .collect::<Vec<_>>(),
                counts
            );
            assert_eq!(plan.len(), if present.is_empty() { 0 } else { 3 });
            assert_eq!(config.hardware.remembered_boards.len(), 3);
        }
        assert_eq!(
            config.hardware.boards[0].automatic.start_frequency_mhz,
            1460.
        );
        for board in &config.hardware.boards {
            let saved = original
                .hardware
                .boards
                .iter()
                .find(|b| b.id == board.id)
                .unwrap();
            assert_eq!(saved.serial, board.serial);
            assert_eq!(
                serde_json::to_value(&saved.automatic).unwrap(),
                serde_json::to_value(&board.automatic).unwrap()
            );
        }
    }
    #[test]
    fn topology_requires_three_consistent_successes_and_ignores_order() {
        let current = configured();
        let one = connected_plan(current.clone(), devices(&[0])).unwrap();
        let two = connected_plan(current.clone(), devices(&[0, 1])).unwrap();
        let mut debounce = TopologyChange::default();
        let mut now = std::time::Instant::now();
        let mut observe = |candidate| {
            now += std::time::Duration::from_secs(5);
            debounce.observe_at(&current, candidate, now)
        };
        assert!(observe(Ok(one.clone())).is_none());
        assert!(observe(Ok(one.clone())).is_none());
        assert!(observe(Err(anyhow::anyhow!("busy"))).is_none());
        assert!(observe(Ok(one.clone())).is_none());
        assert!(observe(Ok(two)).is_none());
        assert!(observe(Ok(one.clone())).is_none());
        assert!(observe(Ok(current.clone())).is_none());
        let mut reordered = current.clone();
        reordered.hardware.boards.reverse();
        assert!(observe(Ok(reordered)).is_none());
        assert!(observe(Ok(one.clone())).is_none());
        assert!(observe(Ok(one.clone())).is_none());
        assert_eq!(observe(Ok(one)).unwrap().hardware.boards.len(), 1);
    }
    #[test]
    fn missed_poll_ticks_cannot_shorten_the_debounce_period() {
        let current = configured();
        let one = connected_plan(current.clone(), devices(&[0])).unwrap();
        let mut change = TopologyChange::default();
        let now = std::time::Instant::now();
        for _ in 0..4 {
            assert!(change.observe_at(&current, Ok(one.clone()), now).is_none());
        }
        assert!(
            change
                .observe_at(
                    &current,
                    Ok(one.clone()),
                    now + std::time::Duration::from_secs(9)
                )
                .is_none()
        );
        assert!(
            change
                .observe_at(&current, Ok(one), now + std::time::Duration::from_secs(10))
                .is_some()
        );
    }
    #[test]
    fn ambiguous_serial_is_not_a_topology_change() {
        assert!(connected_plan(configured(), devices(&[0, 0])).is_err());
    }
    #[test]
    fn explicit_mapping_keeps_absent_board_while_refreshing_connected_locations() {
        let mut config = Config::default();
        config.hardware.boards = (0..3).map(|i| toml::from_str(&format!(
            "id='b{i}'\nname='B{i}'\nserial='serial-{i}'\ndevice_location={}\nclient_port={}", i+1, i+18110
        )).unwrap()).collect();
        config.hardware.chain_assignments = Some(timelord_core::ChainAssignments {
            challenge: "serial-2".into(),
            reward: "serial-0".into(),
            infused_challenge: "serial-1".into(),
        });
        let expected = config.assignments().unwrap();
        let devices = vec![Device {
            name: "Chia VDF A".into(),
            location: 99,
            serial: Some("serial-2".into()),
        }];
        let plan = connected_plan(config.clone(), devices).unwrap();
        assert_eq!(plan.assignments().unwrap(), expected);
        assert_eq!(plan.hardware.boards.len(), 3);
        assert_eq!(plan.hardware.boards[2].device_location, Some(99));
        assert_eq!(
            connected_plan(config, vec![])
                .unwrap()
                .assignments()
                .unwrap(),
            expected
        );
    }
    #[test]
    fn hotplug_follows_the_serial_and_never_a_replacement_at_the_old_port() {
        let board = toml::from_str(
            "id='a'\nname='A'\nclient_port=18110\ndevice_location=1\nserial='known'",
        )
        .unwrap();
        let other = Device {
            name: "Chia VDF A".into(),
            location: 1,
            serial: Some("other".into()),
        };
        assert!(board_location(&board, &[]).is_err());
        assert!(board_location(&board, std::slice::from_ref(&other)).is_err());
        let moved = Device {
            location: 9,
            serial: Some("known".into()),
            ..other.clone()
        };
        assert_eq!(board_location(&board, &[other, moved.clone()]).unwrap(), 9);
        assert!(board_location(&board, &[moved.clone(), moved]).is_err());
    }
}
