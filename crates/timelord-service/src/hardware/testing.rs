//! Multi-board fault rig: scripted fake native clients, a fixture USB inventory,
//! the real board supervisors and the real routing loop. No USB, node or ports.
use super::*;
use std::path::PathBuf;

/// A POSIX-shell native client. Files in its directory inject faults:
/// `exit-<loc>` exits before acknowledging, `busy-<loc>` cannot open its USB
/// device (still held by a previous owner), `stall-<loc>` stops all output and
/// control (a blocked USB write), `hot-<loc>` reports 60°C and `burst-<loc>`
/// emits one burst of 25 engine restarts and `die-<loc>` kills a running
/// client (as a cable pull does).
const CLIENT: &str = r#"#!/bin/sh
root=$(dirname "$0")
case "$1" in
 --control-version) echo timelord-live-control-v1; exit 0;;
 --control-frequencies) echo '[1400,1410,1420,1430,1440,1450,1460,1470,1480,1490,1500]'; exit 0;;
esac
loc=0
while [ "$#" -gt 0 ]; do
 if [ "$1" = --device-loc ]; then loc=$2; fi
 shift
done
echo $$ >> "$root/launches-$loc"
if [ -f "$root/exit-$loc" ]; then exit 1; fi
if [ -f "$root/busy-$loc" ]; then
 echo "FT_OpenEx(spi_loc_id, FT_OPEN_BY_LOCATION, &spi_ft_handle) failed (error 3)" >&2
 echo "Failed to open device" >&2
 exit 1
fi
(
 while :; do
  if [ -f "$root/stall-$loc" ]; then sleep 0.05; continue; fi
  if [ -f "$root/die-$loc" ]; then kill $$; exit 1; fi
  if [ -f "$root/hot-$loc" ]; then t=60.0; else t=40.0; fi
  printf 'ASIC Temp = %s C; Frequency = 1450.0 MHz; freq_idx = 1\n' "$t" >&2
  printf 'TLRAW 2 1 1000000 running\n' >&2
  if [ -f "$root/burst-$loc" ]; then
   rm -f "$root/burst-$loc"
   n=0; while [ "$n" -lt 25 ]; do printf 'VDF 1: Restarting VDF at 123 iters\n' >&2; n=$((n + 1)); done
  fi
  sleep 0.05
 done
) &
telemetry=$!
trap 'kill $telemetry 2>/dev/null; exit 0' TERM INT
connected=0
while read command version id frequency voltage; do
 while [ -f "$root/stall-$loc" ]; do sleep 0.05; done
 if [ "$command" = TLSTOP ]; then
  kill $telemetry 2>/dev/null
  printf 'TLSTOPACK 1 %s 1 1\n' "$id"
  printf 'Hardware shutdown: engines_paused=1 safe_point_restored=1\n' >&2
  exit 0
 fi
 echo "$id $frequency $voltage" >> "$root/commands-$loc"
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
 if [ "$connected" = 0 ]; then
  connected=1
  printf 'VDF 0: Connected to timelord, waiting for challenge\n' >&2
 fi
done
kill $telemetry 2>/dev/null
"#;

pub(crate) struct Rig {
    prefix: String,
    _exclusive: tokio::sync::MutexGuard<'static, ()>,
    pub dir: PathBuf,
    pub shared: Arc<Shared>,
    pub store: Arc<crate::safety::Store>,
    stop: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Rig {
    /// `count` boards with serials `S1..`, USB locations `1..` and the given
    /// profile. Known boards (`known`) start without a cautious start.
    /// Serials are unique per rig: USB leases are host-wide.
    pub fn serial(&self, index: u32) -> String {
        format!("{}{index}", self.prefix)
    }
    pub async fn new(name: &str, count: u32, profile: &str, known: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        // Rigs run one at a time: each runs several shell clients emitting
        // telemetry, and timing assertions need an unloaded host.
        static EXCLUSIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let exclusive = EXCLUSIVE.lock().await;
        let prefix: String = name
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect();
        let dir = std::env::temp_dir().join(format!(
            "tl-rig-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(&binary, CLIENT).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::default();
        config.hardware.client_binary = Some(binary.clone());
        config.hardware.automatic_state = Some(dir.join("automatic-profiles.json"));
        config.hardware.boards = (1..=count)
            .map(|i| {
                toml::from_str(&format!(
                    "id='b{i}'\nname='ASIC {i}'\nserial='{prefix}{i}'\nclient_port={}\ndevice_location={i}\nprofile='{profile}'\nfrequency_mhz=1450.0\nvoltage_v=1.0",
                    18110 + i
                ))
                .unwrap()
            })
            .collect();
        crate::inventory::fixture::set(
            binary,
            (1..=count)
                .map(|i| crate::inventory::Device {
                    name: format!("ASIC {i}"),
                    location: i,
                    serial: Some(format!("{prefix}{i}")),
                })
                .collect(),
        );
        let store = Arc::new(crate::safety::Store::open(&dir.join("config.toml")).unwrap());
        if known {
            for i in 1..=count {
                let serial = format!("{prefix}{i}");
                let token = store
                    .begin_owner(
                        &serial,
                        Point {
                            frequency_mhz: 1450.,
                            voltage_v: 1.,
                        },
                    )
                    .unwrap();
                assert!(store.clear_cautious(&serial, token).unwrap());
                store.closed(&serial).unwrap();
            }
        }
        let native = if profile == "automatic" {
            Some(Facts::load(&config).await.unwrap())
        } else {
            None
        };
        let mut runtime = crate::runtime::RuntimeState::new(false);
        restore_lifecycles(&config, &mut runtime.boards, &store);
        runtime.safety = Some(store.clone());
        // No node: optional rebalancing is always allowed.
        runtime.scheduler_initialized = false;
        let shared = Arc::new(Shared::new(
            native,
            config,
            Arc::new(std::sync::Mutex::new(runtime)),
            crate::operational::State::fixture(),
            crate::unix_now(),
            crate::unix_now(),
        ));
        let (stop, _) = watch::channel(false);
        Self {
            prefix,
            _exclusive: exclusive,
            dir,
            shared,
            store,
            stop,
            tasks: vec![],
        }
    }
    pub fn fast() -> super::board::Timing {
        super::board::Timing {
            restart: [Duration::from_millis(50); 3],
            absent: Duration::from_millis(100),
            liveness: Duration::from_millis(1500),
            canary: Duration::from_millis(500),
        }
    }
    /// Start every board supervisor and the routing loop.
    pub fn start(&mut self, timing: super::board::Timing) {
        let backend = fixture_backend(&self.shared);
        for board in self.shared.config.hardware.boards.clone() {
            self.tasks
                .push(tokio::spawn(super::board::supervise_board_with(
                    self.shared.clone(),
                    backend.clone(),
                    board,
                    self.stop.subscribe(),
                    timing,
                )));
        }
        let shared = self.shared.clone();
        let mut stop = self.stop.subscribe();
        self.tasks.push(tokio::spawn(async move {
            let mut rebalance = super::continuous::Rebalance::default();
            loop {
                super::continuous::reconcile(&shared, &mut rebalance).unwrap();
                tokio::select! {
                    _ = stop.changed() => return,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        }));
    }
    pub fn touch(&self, name: &str) {
        std::fs::write(self.dir.join(name), "").unwrap();
    }
    pub fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.dir.join(name));
    }
    pub fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.join(name))
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
    pub async fn until(
        &self,
        seconds: u64,
        what: &str,
        condition: impl Fn(&crate::runtime::RuntimeState) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            if condition(&self.shared.runtime.lock().unwrap()) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                let state = self.shared.runtime.lock().unwrap();
                let boards: Vec<_> = state
                    .boards
                    .iter()
                    .map(|(id, h)| {
                        format!(
                            "{id}: {} {:?} {:?}",
                            h.lifecycle.status().phase.as_str(),
                            h.recovery_reason,
                            h.lifecycle.status().fault_reason
                        )
                    })
                    .collect();
                panic!(
                    "timed out waiting for {what}: {boards:#?} routes={:?}",
                    state.routing
                )
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for task in self.tasks {
            tokio::time::timeout(Duration::from_secs(20), task)
                .await
                .expect("rig task stops")
                .unwrap();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn serving(state: &crate::runtime::RuntimeState, id: &str) -> bool {
    state
        .boards
        .get(id)
        .is_some_and(|b| b.lifecycle.accepts_work())
}
pub(crate) fn routed(state: &crate::runtime::RuntimeState, id: &str) -> bool {
    state.routing.iter().any(|a| a.board_id == id)
}
