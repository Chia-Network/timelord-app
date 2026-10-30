//! Read-only D2XX enumeration. Never opens a USB handle or initializes an ASIC.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::{process::Stdio, time::Duration};
use timelord_core::Config;
#[cfg(any(target_os = "macos", test))]
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub name: String,
    pub location: u32,
    pub serial: Option<String>,
}

#[cfg(target_os = "macos")]
pub fn native(path: &Path) -> Result<Vec<Device>> {
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
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    // D2XX ABI from the distributed macOS ftd2xx.h/WinTypes.h: DWORD/ULONG
    // are unsigned 32-bit, with a pointer-sized handle. Only enumeration exports
    // are resolved; neither FT_Open nor any control/write export is called.
    unsafe {
        // Only the disposable probe process enters this function. Leave the
        // driver loaded until process exit; it owns background USB threads.
        let library = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() {
            bail!("Could not load the native client's D2XX library");
        }
        let create = libc::dlsym(library, c"FT_CreateDeviceInfoList".as_ptr());
        let get = libc::dlsym(library, c"FT_GetDeviceInfoList".as_ptr());
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

#[cfg(not(target_os = "macos"))]
pub fn native(_path: &Path) -> Result<Vec<Device>> {
    bail!("Native USB discovery requires macOS")
}

/// Never load D2XX into the long-lived proof service. A timed-out blocking
/// thread cannot be cancelled and may keep using the driver after timeout.
#[cfg(any(target_os = "macos", test))]
async fn probe(helper: &Path, driver: &Path, timeout: Duration) -> Result<Vec<Device>> {
    let mut child = Command::new(helper)
        .arg("usb-inventory")
        .arg("--driver-library")
        .arg(driver)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Starting isolated USB discovery")?;
    let stdout = child.stdout.take().context("Discovery stdout missing")?;
    let stderr = child.stderr.take().context("Discovery stderr missing")?;
    async fn read(
        source: impl tokio::io::AsyncRead + Unpin,
        limit: u64,
    ) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        source.take(limit + 1).read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > limit {
            return Err(std::io::Error::other(
                "USB discovery helper output exceeded its limit",
            ));
        }
        Ok(bytes)
    }
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(child.wait(), read(stdout, 256 * 1024), read(stderr, 4096))
    })
    .await;
    match result {
        Ok(Ok((status, output, diagnostics))) => {
            if !status.success() {
                bail!(
                    "USB discovery helper stopped ({status}): {}",
                    String::from_utf8_lossy(&diagnostics)
                );
            }
            let devices: Vec<Device> =
                serde_json::from_slice(&output).context("Invalid USB discovery response")?;
            if devices.len() > 256 {
                bail!("USB discovery returned too many devices");
            }
            Ok(devices)
        }
        result => {
            // Do not accumulate stuck driver threads or orphan probe processes.
            let _ = child.kill().await;
            let _ = child.wait().await;
            match result {
                Err(_) => bail!("USB enumeration timed out; discovery helper terminated"),
                Ok(Err(error)) => Err(error).context("Reading USB discovery response"),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

pub async fn discover(config: &Config) -> Result<Vec<Device>> {
    #[cfg(target_os = "macos")]
    {
        let helper = std::env::current_exe()?;
        let path = driver_path(config, &helper);
        if path.is_file() {
            return probe(&helper, &path, Duration::from_secs(10)).await;
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
        let port = (18110..=u16::MAX)
            .find(|p| {
                *p != config.service.status_listen.port()
                    && *p != config.service.supervisor_observability_listen.port()
                    && !config
                        .service
                        .observability_listen
                        .is_some_and(|a| a.port() == *p)
                    && !config.hardware.boards.iter().any(|b| b.client_port == *p)
            })
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
    let progress = shared.runtime.lock().expect("runtime").progress.clone();

    let _discovery = progress.as_ref().map(|p| {
        p.begin(
            timelord_supervisor::progress::Resource::Topology,
            0,
            0,
            timelord_supervisor::progress::Stage::Discovery,
        )
    });
    // Only confirm admissions that were already pending when this probe began.
    let pending: std::collections::HashMap<_, _> = shared
        .runtime
        .lock()
        .expect("runtime")
        .boards
        .iter()
        .filter_map(|(id, h)| {
            h.lifecycle
                .admitted_inventory_epoch()
                .map(|epoch| (id.clone(), epoch))
        })
        .collect();
    let mut devices = discover(&shared.config).await?;
    {
        let mut state = shared.runtime.lock().expect("runtime");
        for board in &shared.config.hardware.boards {
            if let Some(hardware) = state.boards.get_mut(&board.id)
                && let Some(epoch) = pending.get(&board.id)
            {
                confirm_admitted_presence(hardware, board, &devices, *epoch);
            }
        }
    }
    preserve_managed_boards(
        &shared.config,
        &mut devices,
        &shared.runtime.lock().expect("runtime"),
        crate::unix_now(),
    );
    connected_plan(shared.config.clone(), devices)
}

pub(crate) fn confirm_board_presence(
    hardware: &mut crate::runtime::HardwareState,
    board: &timelord_core::BoardConfig,
    devices: &[Device],
) {
    let matches = devices
        .iter()
        .filter(|d| d.serial.is_some() && d.serial == board.serial)
        .count();
    hardware
        .lifecycle
        .inventory(None, inventory_observation(matches));
}

fn confirm_admitted_presence(
    hardware: &mut crate::runtime::HardwareState,
    board: &timelord_core::BoardConfig,
    devices: &[Device],
    epoch: u64,
) {
    let matches = devices
        .iter()
        .filter(|d| d.serial.is_some() && d.serial == board.serial)
        .count();
    hardware
        .lifecycle
        .inventory(Some(epoch), inventory_observation(matches));
}

fn inventory_observation(matches: usize) -> crate::board_lifecycle::Inventory {
    match matches {
        0 => crate::board_lifecycle::Inventory::Missing,
        1 => crate::board_lifecycle::Inventory::Present,
        _ => crate::board_lifecycle::Inventory::Ambiguous,
    }
}

fn preserve_managed_boards(
    config: &Config,
    devices: &mut Vec<Device>,
    runtime: &crate::runtime::RuntimeState,
    now: u64,
) {
    // Open FTDI handles can hide their metadata even while a socket or sensor
    // watchdog is recovering the client. Health is not physical absence. Keep
    // a known-present board until its supervisor has closed the native owner
    // and obtained a successful inventory that actually omits that serial.
    for board in &config.hardware.boards {
        if let Some(state) = runtime.boards.get(&board.id)
            && state.lifecycle.preserve_in_inventory(
                state.temperature_unix > 0
                    && now.saturating_sub(state.temperature_unix)
                        <= crate::hardware::TEMPERATURE_TIMEOUT_SECONDS,
            )
            && !devices.iter().any(|d| {
                (d.serial.is_some() && d.serial == board.serial)
                    || Some(d.location) == state.device_location.or(board.device_location)
            })
            && let Some(location) = state.device_location.or(board.device_location)
        {
            devices.push(Device {
                name: board.name.clone(),
                serial: board.serial.clone(),
                location,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn initialization_wait_does_not_arm_discovery_watchdog() {
        let metrics = crate::observability::Metrics::new(timelord_core::Network::Mainnet).unwrap();
        let mut config = Config::default();
        config.hardware.client_binary = Some("/not-present/client".into());
        config.hardware.driver_library = Some("/not-present/driver".into());
        let state = crate::runtime::RuntimeState {
            progress: Some(metrics.progress.clone()),
            metrics: Some(metrics.clone()),
            ..Default::default()
        };
        let shared = crate::Shared {
            config,
            assignments: vec![],
            clients: Default::default(),
            runtime: std::sync::Arc::new(std::sync::Mutex::new(state)),
            started_unix: 0,
            service_started_unix: 0,
        };
        let initialization = crate::hardware::INITIALIZATION.lock().await;
        let pending = live_configuration(&shared);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        assert!(metrics.progress.snapshot(0, 1).unwrap().work[8].is_none());
        drop(initialization);
        assert!(pending.await.is_err());
        assert!(metrics.progress.snapshot(0, 2).unwrap().work[8].is_none());
    }

    #[test]
    fn admitted_capacity_requires_matching_fresh_inventory_despite_display_changes() {
        let config = configured();
        let board = &config.hardware.boards[0];
        {
            let mut h = crate::runtime::HardwareState {
                lifecycle: {
                    let mut owner = crate::board_lifecycle::BoardLifecycle::default();
                    owner.admitted(7);
                    owner
                },
                ..Default::default()
            };
            let blocked = |h: &crate::runtime::HardwareState| {
                crate::hardware::unavailable_for(
                    &config,
                    &std::collections::HashMap::from([(board.id.clone(), h.clone())]),
                    None,
                )
                .contains(&board.id)
            };
            assert!(blocked(&h));
            confirm_admitted_presence(&mut h, board, &devices(&[0]), 6);
            assert!(blocked(&h), "old inventory released a newer admission");
            confirm_admitted_presence(&mut h, board, &[], 7);
            assert!(blocked(&h));
            let mut duplicate = devices(&[0]);
            duplicate.extend(devices(&[0]));
            confirm_admitted_presence(&mut h, board, &duplicate, 7);
            assert!(blocked(&h), "ambiguous inventory released capacity");
            confirm_admitted_presence(&mut h, board, &devices(&[0]), 7);
            assert!(!blocked(&h));
            assert_eq!(h.lifecycle.status().usb_present, Some(true));
        }
    }

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
    fn admitted_board_unplug_during_debounce_removes_capacity_and_presence_fallback() {
        let config = configured();
        let board = &config.hardware.boards[0];
        let mut state = crate::runtime::RuntimeState::default();
        let mut hardware = crate::runtime::HardwareState::default();
        hardware.lifecycle.begin_attempt(1);
        hardware.lifecycle.admitted(7);
        confirm_admitted_presence(&mut hardware, board, &devices(&[0]), 7);
        assert!(hardware.lifecycle.capacity(2, true).usable);
        assert_eq!(hardware.lifecycle.admitted_inventory_epoch(), Some(7));
        let epoch = hardware.lifecycle.admitted_inventory_epoch().unwrap();
        confirm_admitted_presence(&mut hardware, board, &[], epoch);
        assert!(!hardware.lifecycle.capacity(3, true).usable);
        assert_eq!(hardware.lifecycle.status().usb_present, Some(false));
        state.boards.insert(board.id.clone(), hardware);
        let mut empty = vec![];
        preserve_managed_boards(&config, &mut empty, &state, 3);
        assert!(empty.is_empty());
        let hardware = state.boards.get_mut(&board.id).unwrap();
        hardware.lifecycle.start_native();
        confirm_admitted_presence(hardware, board, &devices(&[0]), epoch);
        assert_eq!(hardware.lifecycle.status().usb_present, Some(false));
    }
    #[test]
    fn automatic_ports_skip_http_listeners_across_hotplug() {
        let mut config = Config::default();
        config.service.status_listen = "127.0.0.1:18111".parse().unwrap();
        config.service.observability_listen = Some("[::]:18110".parse().unwrap());
        config.service.supervisor_observability_listen = "127.0.0.1:18112".parse().unwrap();
        config.validate().unwrap();
        for present in [&[0, 1, 2][..], &[], &[0], &[0, 1, 2]] {
            config = connected_plan(config, devices(present)).unwrap();
            config.validate().unwrap();
            assert_eq!(config.hardware.boards.len(), present.len());
            let ports: std::collections::BTreeSet<_> = config
                .hardware
                .boards
                .iter()
                .map(|b| b.client_port)
                .collect();
            assert_eq!(ports.len(), present.len());
            assert!(!ports.contains(&18110));
            assert!(!ports.contains(&18111));
            assert!(!ports.contains(&18112));
            assert_eq!(
                config.assignments().unwrap().len(),
                if present.is_empty() { 0 } else { 3 }
            );
        }
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
    fn visible_managed_board_is_not_synthesized_twice() {
        let config = configured();
        let mut runtime = crate::runtime::RuntimeState::default();
        let mut board = crate::runtime::HardwareState::default();
        board
            .lifecycle
            .inventory(None, crate::board_lifecycle::Inventory::Present);
        runtime.boards.insert("b0".into(), board);
        let mut detected = devices(&[0, 1, 2]);
        preserve_managed_boards(&config, &mut detected, &runtime, 1000);
        assert_eq!(detected.len(), 3);
        assert_eq!(
            connected_plan(config, detected)
                .unwrap()
                .hardware
                .boards
                .len(),
            3
        );
    }

    #[test]
    fn idle_owner_without_socket_keeps_three_board_layout_through_metadata_gaps() {
        let config = configured();
        let mut runtime = crate::runtime::RuntimeState::default();
        runtime.boards.insert(
            "b2".into(),
            crate::runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
                    crate::board_lifecycle::Phase::Connected,
                    false,
                    None,
                    Some(true),
                    0,
                ),
                temperature_unix: 1000,
                device_location: Some(3),
                ..Default::default()
            },
        );
        let mut topology = TopologyChange::default();
        let instant = std::time::Instant::now();
        // No socket or recent raw IPS is required for an idle, supervised owner.
        for elapsed in [0, 10, 20, 30, 45, 59, 60, 90, 300] {
            let mut detected = devices(&[0, 1]);
            preserve_managed_boards(&config, &mut detected, &runtime, 1000 + elapsed);
            let next = connected_plan(config.clone(), detected).unwrap();
            assert_eq!(next.hardware.boards.len(), 3);
            assert!(
                topology
                    .observe_at(&config, Ok(next), instant + Duration::from_secs(elapsed))
                    .is_none()
            );
        }
    }

    #[test]
    fn transport_and_sensor_recovery_do_not_become_false_usb_removals() {
        let config = configured();
        for phase in [
            crate::board_lifecycle::Phase::Connected,
            crate::board_lifecycle::Phase::Fault,
            crate::board_lifecycle::Phase::Reconnecting,
            crate::board_lifecycle::Phase::Starting,
        ] {
            let mut runtime = crate::runtime::RuntimeState::default();
            runtime.boards.insert(
                "b2".into(),
                crate::runtime::HardwareState {
                    lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
                        phase,
                        false,
                        None,
                        Some(true),
                        0,
                    ),
                    temperature_unix: 1000,
                    ..Default::default()
                },
            );
            let mut detected = devices(&[0, 1]);
            preserve_managed_boards(&config, &mut detected, &runtime, 2000);
            assert_eq!(
                connected_plan(config.clone(), detected)
                    .unwrap()
                    .hardware
                    .boards
                    .len(),
                3
            );
        }
    }

    #[test]
    fn closed_owner_confirmed_absence_wins_over_old_socket_or_telemetry() {
        let config = configured();
        let mut runtime = crate::runtime::RuntimeState::default();
        runtime.boards.insert(
            "b2".into(),
            crate::runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
                    crate::board_lifecycle::Phase::Connected,
                    false,
                    None,
                    Some(false),
                    0,
                ),
                temperature_unix: 1000,
                ..Default::default()
            },
        );
        let mut detected = devices(&[0, 1]);
        preserve_managed_boards(&config, &mut detected, &runtime, 1001);
        assert_eq!(
            connected_plan(config, detected)
                .unwrap()
                .hardware
                .boards
                .len(),
            2
        );
    }
    #[test]
    fn absent_faulted_or_stale_owner_does_not_hide_an_unplugged_board() {
        let config = configured();
        for (phase, latched, age) in [
            (crate::board_lifecycle::Phase::Reconnecting, false, 1),
            (crate::board_lifecycle::Phase::Fault, false, 1),
            (crate::board_lifecycle::Phase::Starting, false, 1),
            (crate::board_lifecycle::Phase::Connected, true, 1),
            (crate::board_lifecycle::Phase::Connected, false, 61),
        ] {
            let mut runtime = crate::runtime::RuntimeState::default();
            runtime.boards.insert(
                "b2".into(),
                crate::runtime::HardwareState {
                    lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
                        phase,
                        false,
                        latched.then_some(crate::board_lifecycle::Fault::InvalidProof),
                        None,
                        0,
                    ),
                    temperature_unix: 1000,
                    ..Default::default()
                },
            );
            let mut detected = devices(&[0, 1]);
            preserve_managed_boards(&config, &mut detected, &runtime, 1000 + age);
            let next = connected_plan(config.clone(), detected).unwrap();
            assert_eq!(next.hardware.boards.len(), 2);
            assert_eq!(
                next.assignments()
                    .unwrap()
                    .iter()
                    .filter(|a| a.board_id == "b0")
                    .count(),
                2
            );
        }
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

#[cfg(all(test, unix))]
mod probe_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "usb-probe-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn helper(&self, name: &str, body: &str) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn crashed_or_malformed_probe_does_not_prevent_the_next_inventory() {
        let fixture = Fixture::new();
        for body in ["kill -KILL $$", "printf 'not JSON'", "printf '[1]'"] {
            let helper = fixture.helper("bad", body);
            assert!(
                probe(&helper, Path::new("unused"), Duration::from_secs(2))
                    .await
                    .is_err()
            );
        }
        let helper = fixture.helper(
            "good",
            r#"test "$1" = usb-inventory && test "$2" = --driver-library && test "$3" = unused || exit 71
printf '%s' '[{"name":"Chia VDF A","location":4113,"serial":"test"}]'"#,
        );
        let found = probe(&helper, Path::new("unused"), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(found[0].serial.as_deref(), Some("test"));
        assert_eq!(found[0].location, 4113);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_timeouts_kill_and_reap_the_driver_process() {
        let fixture = Fixture::new();
        for i in 0..3 {
            let pidfile = fixture.0.join(format!("pid-{i}"));
            let helper = fixture.helper(
                &format!("hang-{i}"),
                &format!("echo $$ > '{}'; exec /bin/sleep 30", pidfile.display()),
            );
            let probing = tokio::spawn(async move {
                probe(&helper, Path::new("unused"), Duration::from_secs(1)).await
            });
            // Establish a real running process before advancing the deadline.
            // CPU pressure in parallel tests must not turn this into a test of
            // killing a helper that has not executed its first instruction yet.
            let startup = std::time::Instant::now();
            let pid: i32 = loop {
                if let Ok(contents) = std::fs::read_to_string(&pidfile)
                    && contents.ends_with('\n')
                    && let Ok(pid) = contents.trim().parse()
                {
                    break pid;
                }
                assert!(
                    !probing.is_finished(),
                    "Probe exited before its fixture started"
                );
                assert!(
                    startup.elapsed() < Duration::from_secs(5),
                    "Probe fixture did not start"
                );
                tokio::task::yield_now().await;
            };
            tokio::time::advance(Duration::from_secs(1)).await;
            let error = probing.await.unwrap().unwrap_err();
            assert!(error.to_string().contains("timed out"), "{error:#}");
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }

    #[tokio::test]
    async fn oversized_probe_output_is_bounded_and_terminated() {
        let fixture = Fixture::new();
        let helper = fixture.helper("flood", "exec /usr/bin/yes oversized");
        let error = probe(&helper, Path::new("unused"), Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("exceeded its limit"),
            "{error:#}"
        );
    }
}
