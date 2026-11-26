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

/// Fixture inventories keyed by native client path, so concurrent tests
/// cannot see each other's boards. Production never consults this.
#[cfg(test)]
pub(crate) mod fixture {
    use super::Device;
    use std::{collections::HashMap, path::PathBuf, sync::Mutex};
    static DEVICES: Mutex<Option<HashMap<PathBuf, Vec<Device>>>> = Mutex::new(None);
    pub fn set(binary: PathBuf, devices: Vec<Device>) {
        DEVICES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(binary, devices);
    }
    pub fn get(binary: &PathBuf) -> Option<Vec<Device>> {
        DEVICES.lock().unwrap().as_ref()?.get(binary).cloned()
    }
}

pub async fn discover(config: &Config) -> Result<Vec<Device>> {
    #[cfg(test)]
    if let Some(devices) = config
        .hardware
        .client_binary
        .as_ref()
        .and_then(fixture::get)
    {
        return Ok(devices);
    }
    #[cfg(target_os = "macos")]
    {
        let helper = std::env::current_exe()?;
        let path = driver_path(config, &helper);
        if path.is_file() {
            return probe(&helper, &path, Duration::from_secs(10)).await;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = config;
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
mod driver_path_tests;

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
        let reserved = config.service.reserved_ports();
        let port = (18110..=u16::MAX)
            .find(|p| {
                !reserved.contains(p) && !config.hardware.boards.iter().any(|b| b.client_port == *p)
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

#[derive(Debug, Clone)]
pub(crate) struct Topology {
    pub boards: Vec<timelord_core::BoardConfig>,
    pub remembered_boards: Vec<timelord_core::BoardConfig>,
}

impl Topology {
    /// Apply inventory-owned fields onto a live configuration. Inventory
    /// owns only the board list and the remembered-board registry.
    pub(crate) fn apply(self, mut live: Config) -> Config {
        live.hardware.boards = self.boards;
        live.hardware.remembered_boards = self.remembered_boards;
        live
    }
}

pub async fn live_configuration(shared: &crate::Shared) -> Result<Option<Topology>> {
    // Explicit administrator mappings retain their fixed chain ownership.
    if shared.config.hardware.client_binary.is_none()
        || shared.config.hardware.chain_assignments.is_some()
    {
        return Ok(None);
    }
    let _initialization = crate::hardware::INITIALIZATION.lock().await;
    // Waiting for a native owner is not a stalled enumeration operation.
    let progress = shared.process.progress.clone();
    let _discovery = progress.begin(
        timelord_supervisor::progress::Resource::Topology,
        0,
        0,
        timelord_supervisor::progress::Stage::Discovery,
    );
    let mut devices = discover(&shared.config).await?;
    preserve_managed_boards(
        &shared.config,
        &mut devices,
        &shared.runtime.lock().expect("runtime"),
        crate::unix_now(),
    );
    let config = connected_plan(shared.config.clone(), devices)?;
    let config = crate::hardware::continuous::known_configuration(config);
    Ok(Some(Topology {
        boards: config.hardware.boards,
        remembered_boards: config.hardware.remembered_boards,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Missing,
    Present,
    Ambiguous,
}

pub(crate) fn board_observation(
    board: &timelord_core::BoardConfig,
    devices: &[Device],
) -> Observation {
    let matches = devices
        .iter()
        .filter(|device| device.serial.is_some() && device.serial == board.serial)
        .count();
    match matches {
        0 => Observation::Missing,
        1 => Observation::Present,
        _ => Observation::Ambiguous,
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
            && let Some(location) = state.device_location.or(board.device_location)
        {
            preserve_device_identity(devices, board, location);
        }
    }
}

// Use a live owner's identity only for an unambiguous anonymous row. A
// reported serial at the same location may belong to a replacement board.
fn preserve_device_identity(
    devices: &mut Vec<Device>,
    board: &timelord_core::BoardConfig,
    location: u32,
) {
    if board.serial.is_some() && devices.iter().any(|d| d.serial == board.serial) {
        return;
    }
    let mut at_location = devices.iter_mut().filter(|d| d.location == location);
    match (at_location.next(), at_location.next()) {
        (None, _) => devices.push(Device {
            name: board.name.clone(),
            serial: board.serial.clone(),
            location,
        }),
        (Some(device), None) if device.serial.is_none() => {
            device.serial.clone_from(&board.serial);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod probe_tests;
