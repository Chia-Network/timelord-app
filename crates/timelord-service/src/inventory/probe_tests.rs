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
