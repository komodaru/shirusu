//! Native SEH cleanup for owned recording state. Rust's unwind ABI alone does
//! not guarantee Drop for Windows structured exceptions.
use std::ffi::c_void;

extern "C-unwind" {
    fn shirusu_invoke_with_finally(
        invoke: unsafe extern "C-unwind" fn(*mut c_void) -> u64,
        context: *mut c_void,
        cleanup: unsafe extern "C" fn(*mut c_void),
    ) -> u64;
}

struct Invocation<F, S> {
    invoke: F,
    original: usize,
    state: Option<S>,
}

/// A value the native helper hands back as its 64-bit result: what an original
/// Win32/COM entry returns (an integer of at most 64 bits or a pointer).
/// `from_word(into_word(x)) == x` (tests::words_round_trip).
pub(crate) trait Word: Copy {
    fn into_word(self) -> u64;
    fn from_word(word: u64) -> Self;
}
macro_rules! int_word {
    ($($t:ty),*) => {$(
        impl Word for $t {
            #[inline(always)]
            fn into_word(self) -> u64 {
                // (sign or zero extension; from_word truncates it back)
                self as u64
            }
            #[inline(always)]
            fn from_word(word: u64) -> Self {
                word as $t
            }
        }
    )*};
}
int_word!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);
impl<T> Word for *mut T {
    #[inline(always)]
    fn into_word(self) -> u64 {
        self.expose_provenance() as u64
    }
    #[inline(always)]
    fn from_word(word: u64) -> Self {
        std::ptr::with_exposed_provenance_mut(word as usize)
    }
}
impl<T> Word for *const T {
    #[inline(always)]
    fn into_word(self) -> u64 {
        self.expose_provenance() as u64
    }
    #[inline(always)]
    fn from_word(word: u64) -> Self {
        std::ptr::with_exposed_provenance(word as usize)
    }
}

/// `invoke` must contain only Copy arguments: a foreign SEH unwind may skip
/// the Rust callback's destructors. Owned state stays in the outer context,
/// where the native finally block takes it exactly once before unwinding out.
/// The result comes back as the helper's own return value; the state comes
/// back as `Some` on every normal return (the finally block takes it only on
/// abnormal termination, after which this function does not return), and a
/// caller given `None` returns the original's result without recording.
pub(crate) fn invoke<F: FnOnce(usize) -> R + Copy, S, R: Word>(
    original: usize,
    invoke: F,
    state: S,
) -> (R, Option<S>) {
    unsafe extern "C-unwind" fn call<F: FnOnce(usize) -> R + Copy, S, R: Word>(
        context: *mut c_void,
    ) -> u64 {
        // SAFETY: `context` is the Invocation that `invoke` below passes, alive and not otherwise borrowed during the call.
        let context = unsafe { &mut *context.cast::<Invocation<F, S>>() };
        (context.invoke)(context.original).into_word()
    }
    unsafe extern "C" fn cleanup<F, S>(context: *mut c_void) {
        // SAFETY: `context` is the same Invocation; the finally block runs after `call` returned or unwound.
        let context = unsafe { &mut *context.cast::<Invocation<F, S>>() };
        let last_error = crate::last_error::get();
        let _guard = crate::runtime::ReentrancyGuard::try_enter();
        drop(context.state.take());
        crate::last_error::set(last_error);
    }
    let mut context = Invocation {
        invoke,
        original,
        state: Some(state),
    };
    // SAFETY: `call` and `cleanup` match the helper's ABI and receive `context`, which outlives the call.
    let word = unsafe {
        shirusu_invoke_with_finally(
            call::<F, S, R>,
            (&mut context as *mut Invocation<F, S>).cast(),
            cleanup::<F, S>,
        )
    };
    (R::from_word(word), context.state.take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountDrop<'a>(&'a AtomicUsize);
    impl Drop for CountDrop<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    extern "C-unwind" {
        fn shirusu_test_raise_seh() -> u64;
        fn shirusu_test_catch_seh(
            invoke: unsafe extern "C-unwind" fn(*mut c_void) -> u64,
            context: *mut c_void,
        ) -> u32;
    }
    #[test]
    fn words_round_trip() {
        for v in [0i32, -1, i32::MIN, i32::MAX, 0x8000_0005u32 as i32] {
            assert_eq!(i32::from_word(v.into_word()), v);
        }
        for v in [0u32, u32::MAX] {
            assert_eq!(u32::from_word(v.into_word()), v);
        }
        for v in [0isize, -1, isize::MIN, isize::MAX] {
            assert_eq!(isize::from_word(v.into_word()), v);
        }
        for v in [0u16, u16::MAX] {
            assert_eq!(u16::from_word(v.into_word()), v);
        }
        let x = 7u8;
        let p = &x as *const u8;
        assert_eq!(<*const u8>::from_word(p.into_word()), p);
        let q = p as *mut u8;
        assert_eq!(<*mut u8>::from_word(q.into_word()), q);
        // SAFETY: the pointer came back with the provenance of `x`, which is alive.
        assert_eq!(unsafe { *<*const u8>::from_word(p.into_word()) }, 7);
    }
    #[test]
    fn normal_return_retains_state_until_caller_drops_it() {
        let drops = AtomicUsize::new(0);
        let (result, state) = invoke(41, |value| value as u64 + 1, CountDrop(&drops));
        assert_eq!(result, 42);
        assert!(state.is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(state);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn rust_panic_cleans_state_exactly_once_and_keeps_payload() {
        let drops = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(|| {
            invoke(
                0,
                |_| -> u64 { panic!("native-finally-payload") },
                CountDrop(&drops),
            );
        });
        assert_eq!(
            result.unwrap_err().downcast_ref::<&str>(),
            Some(&"native-finally-payload")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn structured_exception_cleans_state_once_and_reaches_native_caller() {
        unsafe extern "C-unwind" fn run(context: *mut c_void) -> u64 {
            // SAFETY: the test passes a pointer to its live AtomicUsize.
            let drops = unsafe { &*context.cast::<AtomicUsize>() };
            // SAFETY: `invoke` runs this inside an SEH frame whose caller catches the exception.
            invoke(0, |_| unsafe { shirusu_test_raise_seh() }, CountDrop(drops)).0
        }
        let mut drops = AtomicUsize::new(0);
        // SAFETY: `run` matches the helper's callback ABI and `drops` outlives the call.
        let code = unsafe { shirusu_test_catch_seh(run, (&mut drops as *mut AtomicUsize).cast()) };
        assert_eq!(code, 0xe0424242);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
