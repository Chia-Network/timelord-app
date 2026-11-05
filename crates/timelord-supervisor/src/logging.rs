//! Best-effort diagnostics: bounded queues and chunks; no disk/pipe I/O on callers.
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};
const CHUNK: usize = 4096;
const QUEUE: usize = 128;
pub const LOG_BYTES: u64 = 5 * 1024 * 1024;
#[derive(Default)]
struct Counters {
    dropped: AtomicU64,
    errors: AtomicU64,
    written: AtomicU64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    pub dropped_bytes: u64,
    pub write_errors: u64,
    pub written_bytes: u64,
}
#[derive(Clone)]
pub struct Writer {
    sender: mpsc::SyncSender<Vec<u8>>,
    counters: Arc<Counters>,
}
impl Writer {
    pub fn stderr() -> Self {
        static STDERR: std::sync::OnceLock<Writer> = std::sync::OnceLock::new();
        STDERR
            .get_or_init(|| Self::start(|bytes| std::io::stderr().lock().write_all(bytes)))
            .clone()
    }
    pub fn stderr_stats() -> Stats {
        Self::stderr().stats()
    }
    pub fn rotating(path: PathBuf) -> Self {
        let mut file: Option<File> = None;
        Self::start(move |bytes| {
            let result = (|| {
                if file.is_none() {
                    let opened = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                        .open(&path)?;
                    let meta = opened.metadata()?;
                    if !meta.is_file()
                        || meta.uid() != unsafe { libc::geteuid() }
                        || meta.nlink() != 1
                        || meta.mode() & 0o077 != 0
                    {
                        return Err(io::Error::other(
                            "Diagnostic log must be a private owned regular file",
                        ));
                    }
                    file = Some(opened);
                }
                if file.as_ref().expect("log").metadata()?.len() + bytes.len() as u64 > LOG_BYTES {
                    file.take();
                    let one = path.with_extension("log.1");
                    let two = path.with_extension("log.2");
                    match std::fs::rename(&one, &two) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                    std::fs::rename(&path, &one)?;
                    file = Some(
                        OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .mode(0o600)
                            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                            .open(&path)?,
                    );
                }
                file.as_mut().expect("log").write_all(bytes)
            })();
            if result.is_err() {
                file.take();
            }
            result
        })
    }
    fn start(mut sink: impl FnMut(&[u8]) -> io::Result<()> + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let counters = Arc::new(Counters::default());
        let stats = counters.clone();
        if std::thread::Builder::new()
            .name("timelord-diagnostics".into())
            .spawn(move || {
                while let Ok(bytes) = receiver.recv() {
                    if sink(&bytes).is_err() {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        stats
                            .dropped
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    } else {
                        stats
                            .written
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    }
                }
            })
            .is_err()
        {
            counters.errors.fetch_add(1, Ordering::Relaxed);
        }
        Self { sender, counters }
    }
    pub fn stats(&self) -> Stats {
        Stats {
            dropped_bytes: self.counters.dropped.load(Ordering::Relaxed),
            write_errors: self.counters.errors.load(Ordering::Relaxed),
            written_bytes: self.counters.written.load(Ordering::Relaxed),
        }
    }
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for part in bytes.chunks(CHUNK) {
            if self.sender.try_send(part.to_vec()).is_err() {
                self.counters
                    .dropped
                    .fetch_add(part.len() as u64, Ordering::Relaxed);
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn wait_until(check: impl Fn() -> bool) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !check() {
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[test]
    fn a_stalled_sink_never_blocks_or_grows_the_producer_queue() {
        let (go, wait) = mpsc::channel::<()>();
        let mut writer = Writer::start(move |_| {
            let _ = wait.recv();
            Ok(())
        });
        let bytes = vec![0; CHUNK];
        let start = std::time::Instant::now();
        for _ in 0..(QUEUE + 100) {
            writer.write_all(&bytes).unwrap();
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert!(writer.stats().dropped_bytes > 0);
        drop(go); // retire the fixture sink rather than leak a blocked test thread
    }
    #[test]
    fn enospc_is_reported_without_blocking_or_failing_the_diagnostic_caller() {
        let mut writer = Writer::start(|_| Err(io::Error::from_raw_os_error(libc::ENOSPC)));
        writer.write_all(b"fixture diagnostic").unwrap();
        wait_until(|| {
            let stats = writer.stats();
            stats.write_errors > 0 && stats.dropped_bytes == 18
        });
        assert_eq!(writer.stats().dropped_bytes, 18);
    }
    #[test]
    fn rotation_is_bounded_and_disk_failure_does_not_fail_the_writer() {
        let root = std::env::temp_dir().join(format!("tl-logs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("worker.log");
        let mut writer = Writer::rotating(path.clone());
        let chunk = vec![b'x'; CHUNK];
        let mut sent = 0_u64;
        for _ in 0..(LOG_BYTES / CHUNK as u64 * 4) {
            writer.write_all(&chunk).unwrap();
            sent += CHUNK as u64;
            if sent.is_multiple_of((CHUNK * 32) as u64) {
                wait_until(|| writer.stats().written_bytes + writer.stats().dropped_bytes >= sent);
            }
        }
        wait_until(|| writer.stats().written_bytes + writer.stats().dropped_bytes >= sent);
        assert!(std::fs::metadata(&path).unwrap().len() <= LOG_BYTES);
        assert!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len()
                <= LOG_BYTES
        );
        assert!(
            std::fs::metadata(path.with_extension("log.2"))
                .unwrap()
                .len()
                <= LOG_BYTES
        );
        let mut bad = Writer::rotating(root.join("absent/worker.log"));
        bad.write_all(b"lost").unwrap();
        wait_until(|| {
            let stats = bad.stats();
            stats.write_errors > 0 && stats.dropped_bytes == 4
        });
        assert_eq!(bad.stats().dropped_bytes, 4);
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }
}
