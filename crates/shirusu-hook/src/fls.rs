//! OS-thread exit notification. Windows Rust TLS destructors deliberately
//! skip fiber threads, so use a native TLS callback there. Deleting a fiber
//! must neither close the capture slot nor consume the eventual thread exit.
use std::sync::{Arc, OnceLock};
pub type ThreadExitHandler = Arc<dyn Fn(u64) + Send + Sync>;
static HANDLER: OnceLock<ThreadExitHandler> = OnceLock::new();

fn notify_exit(thread_id: u64) {
    if let Some(handler) = HANDLER.get() {
        handler(thread_id);
    }
}

#[cfg(windows)]
thread_local! {
    // No destructor: the callback runs exactly on DLL_THREAD_DETACH, including
    // when the first registration was performed inside a subsequently deleted fiber.
    // Plain copy TLS: arming is one store, with no allocation and no lock, so
    // it can run inside a Detour that may be terminated (review 2026-11-16 P1).
    static ARMED_THREAD: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Exit notices waiting for the management path. A detach stores its thread
/// ID into a free cell; the drain swaps cells back to 0. When every cell is
/// taken the notice is dropped: such a thread is then found by its liveness
/// (the management path reaps terminated threads), like one ended by
/// `TerminateThread`, which never sends a notice either.
#[cfg(windows)]
const EXIT_CELLS: usize = 1024;
#[cfg(windows)]
static EXITS: [std::sync::atomic::AtomicU64; EXIT_CELLS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; EXIT_CELLS];
/// Set after a notice is stored, so the drain is one load when idle.
#[cfg(windows)]
static ANY_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Run exit handlers outside the native TLS callback. That callback holds the
/// loader lock, so it must not allocate, free, or acquire runtime locks.
/// Call from the recording management path or explicit shutdown.
#[inline]
pub fn drain_pending_exits() {
    #[cfg(windows)]
    {
        use std::sync::atomic::Ordering;
        // The ordinary path only reads the cache line. Avoid an inter-thread
        // read/modify/write when there are no detach notices.
        if ANY_EXIT.load(Ordering::Acquire) {
            drain_pending_exits_slow();
        }
    }
}

#[cfg(windows)]
#[cold]
#[inline(never)]
fn drain_pending_exits_slow() {
    use std::sync::atomic::Ordering;
    // Cleared before the scan: a notice stored after a cell was scanned sets
    // the flag again, so it is seen by the next drain.
    ANY_EXIT.store(false, Ordering::SeqCst);
    for cell in &EXITS {
        if cell.load(Ordering::Relaxed) == 0 {
            continue;
        }
        let thread_id = cell.swap(0, Ordering::Acquire);
        if thread_id != 0 {
            notify_exit(thread_id);
        }
    }
}

/// Store a notice into a free cell (no allocation, no lock). False when all
/// cells are taken.
#[cfg(windows)]
fn post_exit(thread_id: u64) -> bool {
    use std::sync::atomic::Ordering;
    let posted = EXITS.iter().any(|cell| {
        cell.load(Ordering::Relaxed) == 0
            && cell
                .compare_exchange(0, thread_id, Ordering::Release, Ordering::Relaxed)
                .is_ok()
    });
    if posted {
        ANY_EXIT.store(true, Ordering::SeqCst);
    }
    posted
}

#[cfg(windows)]
unsafe extern "system" fn thread_callback(
    _: *mut std::ffi::c_void,
    reason: u32,
    _: *mut std::ffi::c_void,
) {
    const DLL_THREAD_DETACH: u32 = 3;
    if reason == DLL_THREAD_DETACH {
        if !crate::runtime::native_tls_available() {
            return;
        }
        crate::runtime::ReentrancyGuard::mark_thread_exiting();
        let _ = ARMED_THREAD.try_with(|slot| {
            let thread_id = slot.replace(0);
            if thread_id != 0 {
                let _ = post_exit(thread_id);
            }
        });
    }
}

// The PE TLS directory contains the .CRT$XL* callback array. Unlike FLS,
// this callback is tied to OS-thread detach and never to DeleteFiber.
#[cfg(windows)]
#[used]
#[link_section = ".CRT$XLB"]
static THREAD_CALLBACK: unsafe extern "system" fn(
    *mut std::ffi::c_void,
    u32,
    *mut std::ffi::c_void,
) = thread_callback;

#[cfg(not(windows))]
struct ThreadExit(u64);
#[cfg(not(windows))]
impl Drop for ThreadExit {
    fn drop(&mut self) {
        crate::runtime::ReentrancyGuard::mark_thread_exiting();
        notify_exit(self.0);
    }
}
#[cfg(not(windows))]
thread_local! {
    static EXIT: std::cell::RefCell<Option<ThreadExit>> = const { std::cell::RefCell::new(None) };
}
pub fn install_thread_exit_handler(handler: ThreadExitHandler) -> bool {
    let _ = HANDLER.set(handler);
    true
}
pub fn arm_current_thread(thread_id: u64) {
    if thread_id == 0 {
        return;
    }
    #[cfg(windows)]
    let _ = ARMED_THREAD.try_with(|slot| {
        if slot.get() == 0 {
            slot.set(thread_id);
        }
    });
    #[cfg(not(windows))]
    let _ = EXIT.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(ThreadExit(thread_id));
        }
    });
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Posting and draining take no lock and lose no notice; a full table
    /// refuses instead of blocking.
    #[test]
    fn exit_cells_hand_every_notice_over_once() {
        static SEEN: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
        let _ = install_thread_exit_handler(Arc::new(|tid| SEEN.lock().unwrap().push(tid)));
        let base = 0x7000_0000u64;
        // Other tests' threads may post too: fill until the table refuses.
        let posted = (0..=EXIT_CELLS as u64)
            .take_while(|i| post_exit(base + i))
            .count() as u64;
        assert!(posted <= EXIT_CELLS as u64, "a full table refuses");
        assert!(posted > 0);
        drain_pending_exits();
        let mut seen = std::mem::take(&mut *SEEN.lock().unwrap());
        seen.retain(|&t| t >= base);
        seen.sort_unstable();
        assert_eq!(seen, (0..posted).map(|i| base + i).collect::<Vec<_>>());
        assert!(EXITS.iter().all(|c| c.load(Ordering::Relaxed) == 0));
    }
}
