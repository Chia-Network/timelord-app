//! Multi-board fault scenarios through the real supervisors and routing loop.
use crate::hardware::health::Tier;
use crate::hardware::testing::{Rig, routed, serving};
use timelord_core::Chain;

fn chain_board(state: &crate::runtime::RuntimeState, chain: Chain) -> Option<String> {
    state
        .routing
        .iter()
        .find(|a| a.chain == chain)
        .map(|a| a.board_id.clone())
}

/// Regression for the m6mini incident: one board's stuck USB write coincided
/// with engine-restart bursts on the healthy boards. Their tuning applied live
/// clock changes during the disturbance, the changes failed and every owner
/// was lost. Healthy boards must hold their settings and keep their owners.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_board_cannot_cascade_into_its_healthy_peers() {
    let mut rig = Rig::new("cascade", 3, "automatic", true).await;
    rig.start(Rig::fast());
    rig.until(20, "three serving routed boards", |s| {
        ["b1", "b2", "b3"]
            .iter()
            .all(|id| serving(s, id) && routed(s, id))
    })
    .await;
    // Let every policy pass its settling window so a burst would count.
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    let initial: Vec<_> = (1..=3)
        .map(|i| rig.lines(&format!("commands-{i}")).len())
        .collect();
    rig.touch("stall-3");
    rig.until(10, "the stalled owner retiring", |s| {
        !serving(s, "b3") && s.disturbed(crate::unix_now())
    })
    .await;
    rig.touch("burst-1");
    rig.touch("burst-2");
    rig.until(10, "both bursts recorded", |s| {
        s.boards["b1"].recoveries >= 25 && s.boards["b2"].recoveries >= 25
    })
    .await;
    rig.until(15, "the stalled board out of every route", |s| {
        !routed(s, "b3") && !s.boards["b3"].lifecycle.status().quiesced
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    for (index, id) in ["b1", "b2"].iter().enumerate() {
        assert_eq!(
            rig.lines(&format!("commands-{}", index + 1)).len(),
            initial[index],
            "{id} changed its operating point during another board's fault"
        );
        assert_eq!(
            rig.lines(&format!("launches-{}", index + 1)).len(),
            1,
            "{id} lost its native owner"
        );
    }
    {
        let state = rig.shared.runtime.lock().unwrap();
        assert!(serving(&state, "b1") && serving(&state, "b2"));
        for chain in [Chain::Challenge, Chain::Reward, Chain::InfusedChallenge] {
            assert_ne!(chain_board(&state, chain).as_deref(), Some("b3"));
        }
    }
    assert_eq!(rig.store.health(&rig.serial(3)).consecutive_failures, 1);
    assert_eq!(rig.store.health(&rig.serial(1)).consecutive_failures, 0);

    // Control: once the faulty board is out and the disturbance has passed,
    // the same burst on a healthy board is real instability evidence.
    rig.store
        .update_health(&rig.serial(3), false, |h| h.disable(None))
        .unwrap();
    rig.remove("stall-3");
    rig.until(30, "the faulty board fully stopped", |s| {
        s.boards["b3"].native_pid.is_none()
            && s.boards["b3"]
                .recovery_reason
                .as_deref()
                .is_some_and(|r| r.contains("Disabled"))
    })
    .await;
    rig.shared.runtime.lock().unwrap().disturbed_until_unix = 0;
    tokio::time::sleep(std::time::Duration::from_secs(7)).await;
    let before = rig.lines("commands-1").len();
    rig.touch("burst-1");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while rig.lines("commands-1").len() == before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "an undisturbed recovery storm did not back off"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    rig.shutdown().await;
}

/// A board that keeps failing still works between failures, so it keeps
/// carrying work once it serves again; only the chain it carries changes. The
/// healthy boards keep their owners throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeatedly_failing_board_serves_again_on_the_least_critical_chain() {
    let mut rig = Rig::new("flaky", 3, "manual", true).await;
    rig.touch("exit-3");
    rig.start(Rig::fast());
    rig.until(20, "two serving boards carrying every chain", |s| {
        serving(s, "b1") && serving(s, "b2") && s.routing.len() == 3
    })
    .await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while rig.store.health(&rig.serial(3)).consecutive_failures < 3 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        rig.store.health(&rig.serial(3)).tier(crate::unix_now()),
        Tier::Probation
    );
    rig.remove("exit-3");
    rig.until(20, "the failing board carrying work again", |s| {
        serving(s, "b3") && routed(s, "b3")
    })
    .await;
    {
        let state = rig.shared.runtime.lock().unwrap();
        assert_eq!(
            chain_board(&state, Chain::InfusedChallenge).as_deref(),
            Some("b3")
        );
    }
    assert_eq!(rig.lines("launches-1").len(), 1);
    assert_eq!(rig.lines("launches-2").len(), 1);
    // The record survives a worker restart.
    let reopened = crate::safety::Store::open(&rig.dir.join("config.toml")).unwrap();
    assert!(reopened.health(&rig.serial(3)).consecutive_failures >= 3);
    drop(reopened);
    rig.shutdown().await;
}

/// An unplugged board is looked for again without counting as a failure, and
/// its peers are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_absent_board_is_not_a_reliability_failure() {
    let mut rig = Rig::new("absent", 2, "manual", true).await;
    let binary = rig.shared.config.hardware.client_binary.clone().unwrap();
    crate::inventory::fixture::set(
        binary.clone(),
        vec![crate::inventory::Device {
            name: "ASIC 1".into(),
            location: 1,
            serial: Some(rig.serial(1)),
        }],
    );
    rig.start(Rig::fast());
    rig.until(20, "the present board serving", |s| serving(s, "b1"))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(rig.store.health(&rig.serial(2)).consecutive_failures, 0);
    assert!(rig.lines("launches-2").is_empty());
    crate::inventory::fixture::set(
        binary,
        (1..=2)
            .map(|i| crate::inventory::Device {
                name: format!("ASIC {i}"),
                location: i,
                serial: Some(rig.serial(i)),
            })
            .collect(),
    );
    rig.until(20, "the returning board serving", |s| serving(s, "b2"))
        .await;
    assert_eq!(rig.store.health(&rig.serial(2)).consecutive_failures, 0);
    assert_eq!(rig.lines("launches-1").len(), 1);
    rig.shutdown().await;
}

/// Overheating latches a thermal restriction on that board only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overheating_restricts_only_its_own_board() {
    let mut rig = Rig::new("thermal", 2, "manual", true).await;
    rig.start(Rig::fast());
    rig.until(20, "both boards serving", |s| {
        serving(s, "b1") && serving(s, "b2")
    })
    .await;
    rig.touch("hot-1");
    rig.until(10, "a thermal stop", |s| {
        s.boards["b1"].lifecycle.status().phase == crate::board_lifecycle::Phase::ThermalStop
    })
    .await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while rig.store.board(&rig.serial(1)).unwrap().restriction
        != Some(crate::safety::Reason::Thermal)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "restriction not saved"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(serving(&rig.shared.runtime.lock().unwrap(), "b2"));
    assert_eq!(rig.lines("launches-2").len(), 1);
    rig.remove("hot-1");
    rig.shutdown().await;
}

/// A device still held by the previous owner during a takeover cannot be
/// opened for a moment. That is "busy", not a reliability failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_usb_device_is_retried_without_a_reliability_failure() {
    let mut rig = Rig::new("busy", 1, "manual", true).await;
    rig.touch("busy-1");
    rig.start(Rig::fast());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while rig.lines("launches-1").len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "busy board was not retried"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    rig.remove("busy-1");
    rig.until(20, "the board serving", |s| serving(s, "b1"))
        .await;
    assert_eq!(rig.store.health(&rig.serial(1)).consecutive_failures, 0);
    rig.shutdown().await;
}

/// Pulling the cable of a running board kills its native client. A closed-owner
/// inventory then shows the board gone: an unplug, not a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unplugging_a_running_board_is_not_a_reliability_failure() {
    let mut rig = Rig::new("unplug", 2, "manual", true).await;
    rig.start(Rig::fast());
    rig.until(20, "both boards serving", |s| {
        serving(s, "b1") && serving(s, "b2")
    })
    .await;
    let binary = rig.shared.config.hardware.client_binary.clone().unwrap();
    let only = |count: u32| -> Vec<crate::inventory::Device> {
        (1..=count)
            .map(|i| crate::inventory::Device {
                name: format!("ASIC {i}"),
                location: i,
                serial: Some(rig.serial(i)),
            })
            .collect()
    };
    crate::inventory::fixture::set(binary.clone(), only(1));
    rig.touch("die-2");
    rig.until(20, "the unplugged board out of service", |s| {
        !serving(s, "b2")
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(rig.store.health(&rig.serial(2)).consecutive_failures, 0);
    rig.remove("die-2");
    crate::inventory::fixture::set(binary, only(2));
    rig.until(20, "the replugged board serving", |s| serving(s, "b2"))
        .await;
    assert_eq!(rig.store.health(&rig.serial(2)).consecutive_failures, 0);
    assert_eq!(rig.lines("launches-1").len(), 1);
    // A client that dies while its board is still attached is a failure.
    rig.touch("die-1");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while rig.store.health(&rig.serial(1)).consecutive_failures == 0 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    rig.remove("die-1");
    rig.shutdown().await;
}

/// USB enumeration can outlive the I/O error of a pulled cable, so the first
/// check still sees the board and counts the stop. When the restart then finds
/// the board missing, that failure was the unplug and is taken back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_detected_unplug_is_forgiven() {
    let mut rig = Rig::new("lateunplug", 2, "manual", true).await;
    rig.start(super::board::Timing {
        restart: [std::time::Duration::from_secs(2); 3],
        ..Rig::fast()
    });
    rig.until(20, "both boards serving", |s| {
        serving(s, "b1") && serving(s, "b2")
    })
    .await;
    let binary = rig.shared.config.hardware.client_binary.clone().unwrap();
    let only = |count: u32| -> Vec<crate::inventory::Device> {
        (1..=count)
            .map(|i| crate::inventory::Device {
                name: format!("ASIC {i}"),
                location: i,
                serial: Some(rig.serial(i)),
            })
            .collect()
    };
    let wait_failures = |expected: u32| {
        let store = rig.store.clone();
        let serial = rig.serial(2);
        async move {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            while store.health(&serial).consecutive_failures != expected {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "failures never became {expected}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    };
    // Still enumerated when its client dies: counted.
    rig.touch("die-2");
    wait_failures(1).await;
    // Then it disappears before the restart: forgiven.
    crate::inventory::fixture::set(binary.clone(), only(1));
    wait_failures(0).await;
    rig.remove("die-2");
    crate::inventory::fixture::set(binary, only(2));
    rig.until(20, "the replugged board serving", |s| serving(s, "b2"))
        .await;
    assert_eq!(rig.store.health(&rig.serial(2)).consecutive_failures, 0);
    rig.shutdown().await;
}

/// A late-detected unplug may forgive the failure it caused, but it must never
/// overwrite an operator disable that landed after the failure was counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_disable_is_not_undone_by_a_late_unplug_forgive() {
    let mut rig = Rig::new("forgiveop", 2, "manual", true).await;
    rig.start(super::board::Timing {
        restart: [std::time::Duration::from_secs(2); 3],
        ..Rig::fast()
    });
    rig.until(20, "both boards serving", |s| {
        serving(s, "b1") && serving(s, "b2")
    })
    .await;
    let binary = rig.shared.config.hardware.client_binary.clone().unwrap();
    let only = |count: u32| -> Vec<crate::inventory::Device> {
        (1..=count)
            .map(|i| crate::inventory::Device {
                name: format!("ASIC {i}"),
                location: i,
                serial: Some(rig.serial(i)),
            })
            .collect()
    };
    // Still enumerated when its client dies: counted as a late transport stop.
    rig.touch("die-2");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while rig.store.health(&rig.serial(2)).consecutive_failures != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the late transport stop was never counted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Hold the setup gate so the next attempt passes its operator-hold check
    // and then waits here. The disable therefore lands mid-attempt, exactly
    // where a late unplug can still be forgiven.
    let initialization = crate::hardware::INITIALIZATION.lock().await;
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    rig.store
        .update_health(&rig.serial(2), false, |h| h.disable(None))
        .unwrap();
    crate::inventory::fixture::set(binary, only(1));
    drop(initialization);
    rig.until(10, "the operator disable surviving the forgive", |s| {
        !serving(s, "b2")
            && s.boards["b2"]
                .recovery_reason
                .as_deref()
                .is_some_and(|r| r.contains("Disabled"))
    })
    .await;
    let health = rig.store.health(&rig.serial(2));
    assert_eq!(health.disabled_until_unix, Some(u64::MAX));
    assert_eq!(health.tier(crate::unix_now()), Tier::Disabled);
    assert_eq!(
        health.consecutive_failures, 1,
        "forgive must not restore the pre-failure snapshot over the operator"
    );
    rig.shutdown().await;
}
