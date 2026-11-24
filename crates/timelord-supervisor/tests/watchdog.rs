mod common;

use common::Fixture;
use std::{fs, time::Duration};
use timelord_supervisor::{Control, request};
use tokio::time::sleep;

#[tokio::test]
async fn progress_reporter_fixture() {
    if std::env::var_os("TIMELORD_PROGRESS_FIXTURE").is_none() {
        return;
    }
    let progress = std::sync::Arc::new(timelord_supervisor::progress::Progress::default());
    let _reporter = timelord_supervisor::progress::Reporter::from_environment(progress).unwrap();
    loop {
        sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test]
async fn essential_failure_retirement_fixture() {
    let Ok(marker) = std::env::var("TIMELORD_ESSENTIAL_FAILURE_MARKER") else {
        return;
    };
    let progress = std::sync::Arc::new(timelord_supervisor::progress::Progress::default());
    let _reporter =
        timelord_supervisor::progress::Reporter::from_environment(progress.clone()).unwrap();
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    sleep(Duration::from_secs(2)).await;
    progress.fail();
    terminate.recv().await;
    // Simulate native ASIC retirement that a 2 s wait would SIGKILL mid-drain.
    sleep(Duration::from_secs(5)).await;
    fs::write(marker, b"retired").unwrap();
}

#[tokio::test]
async fn reported_essential_failure_keeps_the_normal_graceful_retirement_wait() {
    let fixture = Fixture::new();
    let marker = fixture.0.join("retired");
    let executable = std::env::current_exe().unwrap();
    let helper = fixture.helper(&format!(
        "export TIMELORD_ESSENTIAL_FAILURE_MARKER='{}'\nexec '{}' --exact essential_failure_retirement_fixture --nocapture",
        marker.display(),
        executable.display()
    ));
    let supervisor = fixture.supervisor(&helper);
    fixture
        .status(|s| {
            s.recoveries.iter().any(|r| {
                r.reason
                    .contains("Worker reported an essential task failure")
            })
        })
        .await;
    assert!(
        marker.exists(),
        "the worker was SIGKILLed before its simulated retirement completed"
    );
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(supervisor).await;
}

#[tokio::test]
async fn shutdown_cancels_slow_reload_validation_and_reaps_the_validator() {
    let fixture = Fixture::new();
    let d = fixture.0.display();
    let validator_pid = fixture.0.join("validator.pid");
    let ready = fixture.0.join("ready");
    let helper = fixture.script(
        "worker",
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = validate ]; then\n\
               echo $$ >> '{d}/pids'\n\
               echo $$ > '{d}/validator.pid'\n\
               exec /bin/sleep 10\n\
             fi\n\
             echo $$ >> '{d}/pids'\n\
             trap 'exit 0' TERM\n\
             touch '{d}/ready'\n\
             while :; do sleep 0.05; done\n"
        ),
    );
    let mut supervisor = fixture.supervisor(&helper);
    fixture.status(|s| s.worker_pid.is_some()).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ready.exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let config = fixture.config();
    let reload = tokio::spawn(async move { request(&config, Control::Reload, None).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !validator_pid.exists() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Reload validation did not start");
    let validator: i32 = fs::read_to_string(&validator_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = std::time::Instant::now();
    unsafe {
        assert_eq!(
            libc::kill(supervisor.id().unwrap() as i32, libc::SIGTERM),
            0
        );
    }
    let exit = tokio::time::timeout(Duration::from_secs(3), supervisor.wait())
        .await
        .expect("shutdown waited for Reload validation")
        .unwrap();
    let elapsed = started.elapsed();
    reload.abort();
    let _ = reload.await;
    assert!(exit.success(), "supervisor exit: {exit:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown took {elapsed:?}"
    );
    assert_eq!(unsafe { libc::kill(validator, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[tokio::test]
async fn independent_watchdog_reaps_and_replaces_a_stopped_worker_without_gui() {
    let fixture = Fixture::new();
    let executable = std::env::current_exe().unwrap();
    let helper = fixture.helper(&format!("export TIMELORD_PROGRESS_FIXTURE=1\nexec '{}' --exact progress_reporter_fixture --nocapture", executable.display()));
    let supervisor = fixture.supervisor(&helper);
    let first = fixture.status(|s| s.progress.is_some()).await;
    let pid = first.worker_pid.unwrap() as i32;
    unsafe {
        assert_eq!(libc::kill(pid, libc::SIGSTOP), 0);
    }
    let replacement = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let status = request(&fixture.config(), Control::Status, None)
                .await
                .unwrap();
            if status.worker_pid.is_some_and(|next| next != pid as u32) {
                break status;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("watchdog replacement exceeded 40 s; the normal 25 s graceful wait must not be used");
    assert!(replacement.worker_generation > first.worker_generation);
    assert!(replacement.restarts >= 1);
    assert!(
        replacement
            .recoveries
            .last()
            .unwrap()
            .reason
            .contains("heartbeat"),
        "unexpected restart: {:?}",
        replacement.recoveries
    );
    unsafe {
        assert_eq!(
            libc::kill(pid, 0),
            -1,
            "old worker must be reaped before replacement"
        );
    }
    request(&fixture.config(), Control::Pause, None)
        .await
        .unwrap();
    fixture.terminate(supervisor).await;
}
