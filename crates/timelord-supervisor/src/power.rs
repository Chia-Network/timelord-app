//! Persistent idle-system-sleep assertion. Display sleep remains permitted.
pub struct Assertion {
    id: Option<u32>,
    last_attempt: Option<std::time::Instant>,
    pub error: Option<String>,
    acquire: fn() -> Result<u32, String>,
    release: fn(u32),
}
impl Default for Assertion {
    fn default() -> Self {
        Self {
            id: None,
            last_attempt: None,
            error: None,
            acquire: create,
            release,
        }
    }
}
impl Assertion {
    pub fn active(&self) -> bool {
        self.id.is_some()
    }
    pub fn set(&mut self, running: bool) {
        if !running {
            if let Some(id) = self.id.take() {
                (self.release)(id);
            }
            self.error = None;
            self.last_attempt = None;
            return;
        }
        if self.id.is_some()
            || self
                .last_attempt
                .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
        {
            return;
        }
        self.last_attempt = Some(std::time::Instant::now());
        match (self.acquire)() {
            Ok(id) => {
                self.id = Some(id);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }
}
impl Drop for Assertion {
    fn drop(&mut self) {
        self.set(false);
    }
}
#[cfg(target_os = "macos")]
fn create() -> Result<u32, String> {
    use std::ffi::{c_char, c_void};
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            text: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFRelease(value: *const c_void);
    }
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: *const c_void,
            level: u32,
            name: *const c_void,
            id: *mut u32,
        ) -> i32;
    }
    unsafe {
        let kind = CFStringCreateWithCString(
            std::ptr::null(),
            c"PreventUserIdleSystemSleep".as_ptr(),
            0x08000100,
        );
        let name = CFStringCreateWithCString(
            std::ptr::null(),
            c"Timelord background proof service".as_ptr(),
            0x08000100,
        );
        if kind.is_null() || name.is_null() {
            if !kind.is_null() {
                CFRelease(kind);
            }
            if !name.is_null() {
                CFRelease(name);
            }
            return Err("Cannot allocate sleep assertion description".into());
        }
        let mut id = 0;
        let result = IOPMAssertionCreateWithName(kind, 255, name, &mut id);
        CFRelease(kind);
        CFRelease(name);
        if result == 0 {
            Ok(id)
        } else {
            Err(format!("Sleep prevention unavailable: IOKit {result}"))
        }
    }
}
#[cfg(target_os = "macos")]
fn release(id: u32) {
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionRelease(id: u32) -> i32;
    }
    unsafe {
        IOPMAssertionRelease(id);
    }
}
#[cfg(not(target_os = "macos"))]
fn create() -> Result<u32, String> {
    Err("Idle sleep prevention requires macOS".into())
}
#[cfg(not(target_os = "macos"))]
fn release(_: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ACQUIRED: AtomicUsize = AtomicUsize::new(0);
    static RELEASED: AtomicUsize = AtomicUsize::new(0);
    fn acquire() -> Result<u32, String> {
        ACQUIRED.fetch_add(1, Ordering::Relaxed);
        Ok(7)
    }
    fn retire(id: u32) {
        assert_eq!(id, 7);
        RELEASED.fetch_add(1, Ordering::Relaxed);
    }
    #[test]
    fn run_holds_one_assertion_through_outages_and_pause_or_exit_releases_it() {
        let mut power = Assertion {
            id: None,
            last_attempt: None,
            error: None,
            acquire,
            release: retire,
        };
        power.set(true);
        power.set(true);
        assert!(power.active());
        assert_eq!(ACQUIRED.load(Ordering::Relaxed), 1);
        power.set(false);
        assert!(!power.active());
        assert_eq!(RELEASED.load(Ordering::Relaxed), 1);
        power.set(true);
        drop(power);
        assert_eq!(ACQUIRED.load(Ordering::Relaxed), 2);
        assert_eq!(RELEASED.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn an_unavailable_assertion_is_reported_and_retries_are_throttled() {
        fn unavailable() -> Result<u32, String> {
            Err("fixture failure".into())
        }
        let mut power = Assertion {
            id: None,
            last_attempt: None,
            error: None,
            acquire: unavailable,
            release,
        };
        power.set(true);
        let first = power.last_attempt;
        power.set(true);
        assert_eq!(power.last_attempt, first);
        assert!(power.error.is_some());
        assert!(!power.active());
        power.set(false);
        assert!(power.error.is_none());
        assert!(power.last_attempt.is_none());
    }
}
