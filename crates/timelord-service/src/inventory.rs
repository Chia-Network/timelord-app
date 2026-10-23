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

pub async fn discover(config: &Config) -> Result<Vec<Device>> {
    #[cfg(target_os = "macos")]
    {
        let path = config.hardware.driver_library.clone().or_else(|| {
            config.hardware.client_binary.as_ref().map(|p| {
                p.parent()
                    .unwrap_or(Path::new("."))
                    .join("../Frameworks/libftd2xx.dylib")
            })
        });
        if let Some(path) = path.filter(|p| p.is_file()) {
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

/// Resolve only physically enumerated boards, preserving the stored registry.
/// This changes the launch plan, never the config file or a running service.
pub async fn connected_configuration(config: Config) -> Result<Config> {
    if config.hardware.client_binary.is_none() {
        return Ok(config);
    }
    let devices = discover(&config).await?;
    connected_plan(config, devices)
}

fn connected_plan(mut config: Config, devices: Vec<Device>) -> Result<Config> {
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
    if config.hardware.boards.is_empty() {
        bail!(
            "No available Chia ASICs detected. Check USB connections and that another client is not using them."
        );
    }
    config.validate().map_err(anyhow::Error::msg)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
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
