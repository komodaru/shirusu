//! Typed IUnknown interception for image-backed shared vtables.
//!
//! The object's vptr and all derived slots stay unchanged. Published wrapper
//! identities and original functions survive shutdown for callers that cached
//! a method pointer. Heap/private vtables are deliberately refused: a live COM
//! reference does not establish the lifetime of an arbitrary mutable table.

use crate::{capture::MemoryReader, com::Guid, engine::ProcessMemory, runtime::HookRuntime};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetModuleHandleW, GetProcAddress, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows_sys::Win32::System::Memory::{
    VirtualProtect, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE, PAGE_READONLY,
    PAGE_READWRITE, PAGE_WRITECOPY,
};

const LIMIT: usize = 256;
type Qi = unsafe extern "system-unwind" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
type Ref = unsafe extern "system-unwind" fn(*mut c_void) -> u32;

struct Slot {
    table: usize,
    module: usize,
    originals: [usize; 3],
    owner: Weak<HookRuntime>,
    published: AtomicBool,
    patched: [AtomicBool; 3],
    // Nonzero until the original page protection has actually been restored.
    protection: [AtomicU32; 3],
}

// A Box's address is stable even when the Vec grows. A published Box is never
// removed, including after unload or detach; only unpublished reservations may
// be reclaimed. Hot wrappers load the stable pointer without a registry lock.
static REGISTRY: Mutex<Vec<Option<Box<Slot>>>> = Mutex::new(Vec::new());
static MODULES: [AtomicUsize; LIMIT] = [const { AtomicUsize::new(0) }; LIMIT];
// Even values identify a live reservation; bit zero marks an observed unload.
// Unpublished reuse gets a new nonwrapping value, so an old callback cannot
// invalidate the next reservation. Callbacks never dereference reclaimable Boxes.
static MODULE_EPOCHS: [AtomicU64; LIMIT] = [const { AtomicU64::new(0) }; LIMIT];
static OBSERVER: AtomicUsize = AtomicUsize::new(0);
static OBSERVER_INIT: Mutex<()> = Mutex::new(());
#[repr(C)]
struct DllNotification {
    flags: u32,
    full_name: *const c_void,
    base_name: *const c_void,
    base: *mut c_void,
    size: u32,
}
type RegisterNotification = unsafe extern "system" fn(
    u32,
    unsafe extern "system" fn(u32, *const DllNotification, *mut c_void),
    *mut c_void,
    *mut *mut c_void,
) -> i32;
unsafe extern "system" fn dll_notification(
    reason: u32,
    data: *const DllNotification,
    _: *mut c_void,
) {
    if reason != 2 || data.is_null() {
        return;
    }
    // SAFETY: the loader passes a valid notification record for the duration of the callback (`data` is non-null).
    let base = unsafe { (*data).base as usize };
    if base == 0 {
        return;
    }
    // Loader-lock callback: atomics only, no allocation, locks, Rust TLS or
    // calls into another DLL. Retired published entries remain safely indexed.
    for index in 0..LIMIT {
        let epoch = MODULE_EPOCHS[index].load(Ordering::Acquire);
        if epoch != 0 && MODULES[index].load(Ordering::Acquire) == base {
            let _ = MODULE_EPOCHS[index].compare_exchange(
                epoch,
                epoch | 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}
/// Register the bounded unload observer before installing shared vtable hooks.
/// Call on the cold initialization path, outside the loader lock. The Agent DLL
/// and this callback must remain loaded for the process lifetime, like published
/// wrappers. No module reference is retained merely to keep a vtable alive.
pub fn initialize() -> bool {
    if OBSERVER.load(Ordering::Acquire) != 0 {
        return true;
    }
    let _lock = OBSERVER_INIT.lock().unwrap_or_else(|e| e.into_inner());
    if OBSERVER.load(Ordering::Acquire) != 0 {
        return true;
    }
    // Published wrappers and the loader callback must never outlive this DLL.
    // Pin only our own module, never a COM server or its dependencies.
    let mut own_module = std::ptr::null_mut();
    // SAFETY: FROM_ADDRESS + PIN only looks up and pins this module; `own_module` is a writable local.
    if unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            dll_notification as *const () as *const u16,
            &mut own_module,
        )
    } == 0
    {
        return false;
    }
    let name: Vec<u16> = "ntdll.dll".encode_utf16().chain([0]).collect();
    // SAFETY: the name is a NUL-terminated UTF-16 string that outlives the call.
    let ntdll = unsafe { GetModuleHandleW(name.as_ptr()) };
    if ntdll.is_null() {
        return false;
    }
    let Some(register) =
        // SAFETY: `ntdll` is loaded for the whole process and the name is a NUL-terminated C string.
        (unsafe { GetProcAddress(ntdll, c"LdrRegisterDllNotification".as_ptr().cast()) })
    else {
        return false;
    };
    // SAFETY: LdrRegisterDllNotification has the RegisterNotification signature.
    let register: RegisterNotification = unsafe { std::mem::transmute(register) };
    let mut cookie = std::ptr::null_mut();
    // SAFETY: `dll_notification` matches the callback ABI, this module is pinned so it never unloads, and `cookie` is a writable local.
    let status = unsafe { register(0, dll_notification, std::ptr::null_mut(), &mut cookie) };
    if status >= 0 && !cookie.is_null() {
        OBSERVER.store(cookie as usize, Ordering::Release);
        true
    } else {
        false
    }
}

static SLOTS: [AtomicPtr<Slot>; LIMIT] = [const { AtomicPtr::new(std::ptr::null_mut()) }; LIMIT];

fn slot(index: usize) -> &'static Slot {
    let ptr = SLOTS[index].load(Ordering::Acquire);
    // Only a successfully published wrapper is callable. Publication follows
    // initialization, and its Slot is retained for the process lifetime.
    // SAFETY: `ptr` was published after its Slot was initialised, and published Slots are never freed.
    unsafe { &*ptr }
}

unsafe extern "system-unwind" fn query<const N: usize>(
    this: *mut c_void,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> i32 {
    let saved = slot(N);
    crate::engine::com_method_dispatch_with(
        this as usize,
        0,
        &[iid as usize, out as usize],
        saved.originals[0],
        Some(&saved.owner),
        // SAFETY: `original` is the IUnknown::QueryInterface slot saved from this vtable, with the Qi signature; the arguments are the caller's.
        |original| unsafe { std::mem::transmute::<usize, Qi>(original)(this, iid, out) },
    )
}

unsafe extern "system-unwind" fn reference<const N: usize, const METHOD: usize>(
    this: *mut c_void,
) -> u32 {
    let saved = slot(N);
    crate::engine::com_method_dispatch_with(
        this as usize,
        METHOD as u32,
        &[],
        saved.originals[METHOD],
        Some(&saved.owner),
        // SAFETY: `original` is the AddRef/Release slot saved from this vtable, with the Ref signature.
        |original| unsafe { std::mem::transmute::<usize, Ref>(original)(this) as i32 },
    ) as u32
}

macro_rules! wrappers {
    ($($n:literal),* $(,)?) => {
        static WRAPPERS: [(Qi, Ref, Ref); LIMIT] = [
            $((query::<$n>, reference::<$n, 1>, reference::<$n, 2>)),*
        ];
    };
}
include!("shared_com_slots.inc");

/// IUnknown の 3 メソッド (vtable の 0..3 番)。番号ではなく名前で持つので、3 番以降を
/// 渡す誤りは書けない (以前は `match method` の `_ => unreachable!()` だった)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unknown {
    QueryInterface,
    AddRef,
    Release,
}

impl Unknown {
    const ALL: [Unknown; 3] = [Unknown::QueryInterface, Unknown::AddRef, Unknown::Release];

    /// vtable と Slot の配列での番号
    fn slot(self) -> usize {
        match self {
            Unknown::QueryInterface => 0,
            Unknown::AddRef => 1,
            Unknown::Release => 2,
        }
    }
}

fn wrapper(index: usize, method: Unknown) -> usize {
    match method {
        Unknown::QueryInterface => WRAPPERS[index].0 as usize,
        Unknown::AddRef => WRAPPERS[index].1 as usize,
        Unknown::Release => WRAPPERS[index].2 as usize,
    }
}

fn query_region(address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    // SAFETY: a plain C struct; all-zero bytes are a valid value.
    let mut region = unsafe { std::mem::zeroed() };
    // SAFETY: VirtualQuery only reads page metadata; `region` has the size passed.
    (unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut region,
            std::mem::size_of_val(&region),
        )
    } != 0)
        .then_some(region)
}

fn read_pointer(address: usize) -> Option<usize> {
    let mut bytes = [0; std::mem::size_of::<usize>()];
    ProcessMemory
        .read_exact(address as u64, &mut bytes)
        .then(|| usize::from_le_bytes(bytes))
}

fn preflight(object: usize) -> Result<(usize, usize, [usize; 3]), &'static str> {
    let table = read_pointer(object)
        .filter(|v| *v != 0)
        .ok_or("vtable-unreadable")?;
    if table & (std::mem::align_of::<AtomicUsize>() - 1) != 0 {
        return Err("vtable-unaligned");
    }
    let mut module = 0;
    let mut originals = [0; 3];
    for (method, original) in originals.iter_mut().enumerate() {
        let address = table
            .checked_add(method * std::mem::size_of::<usize>())
            .ok_or("vtable-unreadable")?;
        let region = query_region(address).ok_or("vtable-unreadable")?;
        if region.State != MEM_COMMIT
            || region.Type != MEM_IMAGE
            || !matches!(
                region.Protect,
                PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY
            )
        {
            return Err("vtable-not-image-data");
        }
        if method == 0 {
            module = region.AllocationBase as usize;
        }
        if module == 0 || module != region.AllocationBase as usize {
            return Err("vtable-crosses-module");
        }
        *original = read_pointer(address)
            .filter(|v| *v != 0)
            .ok_or("method-unreadable")?;
    }
    Ok((table, module, originals))
}

// The caller holds a live COM reference (install) or a temporary module
// reference (restore). An aligned pointer never straddles a page, so each
// original protection applies to exactly the page this operation changes.
unsafe fn exchange(saved: &Slot, index: usize, which: Unknown, installing: bool) -> bool {
    let method = which.slot();
    let address = saved.table + method * std::mem::size_of::<usize>();
    let mut old = 0;
    if !make_writable(address, &mut old) {
        return false;
    }
    let pending = saved.protection[method].load(Ordering::Acquire);
    let restore = if pending == 0 { old } else { pending };
    saved.protection[method].store(restore, Ordering::Release);
    let detour = wrapper(index, which);
    let (expected, desired) = if installing {
        (saved.originals[method], detour)
    } else {
        (detour, saved.originals[method])
    };
    // SAFETY: `address` is an aligned vtable slot inside a committed image page that was just made writable, and every writer of it uses atomics.
    let pointer = unsafe { AtomicUsize::from_ptr(address as *mut usize) };
    let exchanged = pointer
        .compare_exchange(expected, desired, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if exchanged && installing {
        saved.published.store(true, Ordering::Release);
        saved.patched[method].store(true, Ordering::Release);
    } else if !installing {
        // A different owner replaced the slot: never overwrite their pointer.
        saved.patched[method].store(false, Ordering::Release);
    }
    let restored = restore_page(address, restore);
    if restored {
        saved.protection[method].store(0, Ordering::Release);
    }
    restored && (exchanged || !installing)
}

#[cfg(test)]
static FAIL_WRITABLE: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FAIL_RESTORE: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
fn injected_failure(counter: &AtomicUsize) -> bool {
    let mut count = counter.load(Ordering::Relaxed);
    loop {
        if count == 0 {
            return false;
        }
        match counter.compare_exchange_weak(count, count - 1, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return true,
            Err(actual) => count = actual,
        }
    }
}
fn make_writable(address: usize, old: &mut u32) -> bool {
    #[cfg(test)]
    if injected_failure(&FAIL_WRITABLE) {
        return false;
    }
    // SAFETY: changing a page's protection does not create references; `old` is a writable location.
    unsafe {
        VirtualProtect(
            address as *const c_void,
            std::mem::size_of::<usize>(),
            PAGE_READWRITE,
            old,
        ) != 0
    }
}
fn restore_page(address: usize, protection: u32) -> bool {
    #[cfg(test)]
    if injected_failure(&FAIL_RESTORE) {
        return false;
    }
    let mut ignored = 0;
    // SAFETY: changing a page's protection does not create references; `ignored` is a writable local.
    unsafe {
        VirtualProtect(
            address as *const c_void,
            std::mem::size_of::<usize>(),
            protection,
            &mut ignored,
        ) != 0
    }
}

fn pending(saved: &Slot) -> bool {
    Unknown::ALL.iter().map(|m| m.slot()).any(|i| {
        saved.patched[i].load(Ordering::Acquire) || saved.protection[i].load(Ordering::Acquire) != 0
    })
}

/// Identity of a published adapter. Published slot identities are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installation {
    pub newly_installed: bool,
    pub table: usize,
    pub slot: u32,
}

pub(crate) fn installation_retired(slot: u32) -> bool {
    MODULE_EPOCHS.get(slot as usize).is_none_or(|epoch| {
        let value = epoch.load(Ordering::Acquire);
        value == 0 || value & 1 != 0
    })
}

/// Install only the three universally typed IUnknown slots.
///
/// # Safety
/// `object` is a valid COM interface with an owned live reference throughout
/// this call. Its first three slots obey the native IUnknown ABI. No concurrent
/// third party may change page protections or unload its implementation module.
pub unsafe fn install(rt: &Arc<HookRuntime>, object: usize) -> Result<Installation, &'static str> {
    let _guard = crate::runtime::ReentrancyGuard::try_enter();
    if OBSERVER.load(Ordering::Acquire) == 0 {
        return Err("module-unload-observer-unavailable");
    }
    if !rt.recording_active() {
        return Err("session-closing");
    }
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if !rt.recording_active() {
        return Err("session-closing");
    }
    let (table, module, originals) = preflight(object)?;
    let owner = Arc::downgrade(rt);
    for (index, entry) in registry.iter().enumerate() {
        if let Some(saved) = entry {
            if saved.table == table
                && saved.owner.ptr_eq(&owner)
                && pending(saved)
                && MODULE_EPOCHS[index].load(Ordering::Acquire) & 1 == 0
            {
                if Unknown::ALL.iter().all(|&m| {
                    originals[m.slot()] == wrapper(index, m)
                        && saved.protection[m.slot()].load(Ordering::Acquire) == 0
                }) {
                    return Ok(Installation {
                        newly_installed: false,
                        table,
                        slot: index as u32,
                    });
                }
                return Err("iunknown-installation-incomplete");
            }
        }
    }
    // A restored wrapper for the same table and originals (an earlier
    // session in this process) keeps its identity: reuse it instead of
    // spending another of the LIMIT slots (2026-11-14 review P3-6). A late
    // caller holding the old wrapper still reaches the same originals.
    let reusable = registry.iter().enumerate().find_map(|(index, entry)| {
        let saved = entry.as_ref()?;
        (saved.table == table
            && saved.module == module
            && saved.originals == originals
            && saved.published.load(Ordering::Acquire)
            && !pending(saved)
            && MODULE_EPOCHS[index].load(Ordering::Acquire) & 1 == 0)
            .then_some(index)
    });
    let index = reusable.unwrap_or_else(|| {
        registry
            .iter()
            .position(Option::is_none)
            .unwrap_or(registry.len())
    });
    if index == LIMIT {
        return Err("iunknown-wrapper-limit");
    }
    let epoch = if reusable.is_some() {
        MODULE_EPOCHS[index].load(Ordering::Acquire)
    } else {
        (MODULE_EPOCHS[index].load(Ordering::Acquire) | 1)
            .checked_add(1)
            .ok_or("iunknown-wrapper-generation-exhausted")?
    };
    let mut saved = Box::new(Slot {
        table,
        module,
        originals,
        owner,
        published: AtomicBool::new(false),
        patched: std::array::from_fn(|_| AtomicBool::new(false)),
        protection: std::array::from_fn(|_| AtomicU32::new(0)),
    });
    let pointer: *mut Slot = &raw mut *saved;
    if reusable.is_some() {
        // The wrapper identity was already published once.
        saved.published.store(true, Ordering::Release);
    }
    if index == registry.len() {
        registry.push(Some(saved));
    } else if let Some(old) = registry[index].replace(saved) {
        // A wrapper may have loaded the old pointer; never free it.
        Box::leak(old);
    }
    MODULES[index].store(module, Ordering::Release);
    MODULE_EPOCHS[index].store(epoch, Ordering::Release);
    SLOTS[index].store(pointer, Ordering::Release);
    // (index の場所には上で置いた Slot がある)
    let Some(saved) = registry.get(index).and_then(Option::as_ref) else {
        return Err("iunknown-wrapper-slot-missing");
    };
    // SAFETY: `saved` was validated by read_table and the caller holds a live COM reference to the object.
    let success = Unknown::ALL
        .iter()
        .all(|&m| unsafe { exchange(saved, index, m, true) });
    if !success {
        for m in Unknown::ALL.into_iter().rev() {
            if saved.patched[m.slot()].load(Ordering::Acquire)
                || saved.protection[m.slot()].load(Ordering::Acquire) != 0
            {
                // SAFETY: same Slot and reference as the install attempt above.
                unsafe {
                    exchange(saved, index, m, false);
                }
            }
        }
        if !saved.published.load(Ordering::Acquire) && !pending(saved) {
            SLOTS[index].store(std::ptr::null_mut(), Ordering::Release);
            MODULES[index].store(0, Ordering::Release);
            registry[index] = None;
        }
        return Err("iunknown-installation-incomplete");
    }
    Ok(Installation {
        newly_installed: true,
        table,
        slot: index as u32,
    })
}

/// Restore owned slots after the recording gate has closed. Keep failures for
/// a later shutdown attempt, including a failed page-protection restoration.
pub fn restore_all() -> usize {
    let _guard = crate::runtime::ReentrancyGuard::try_enter();
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let mut unrestored = 0;
    for (index, entry) in registry.iter_mut().enumerate() {
        let Some(saved) = entry else {
            continue;
        };
        if !pending(saved) {
            continue;
        }
        let mut module = std::ptr::null_mut();
        // SAFETY: FROM_ADDRESS only looks up the module (taking a reference released below); `module` is a writable local.
        let held = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                saved.table as *const u16,
                &mut module,
            )
        } != 0;
        let unloaded = MODULE_EPOCHS[index].load(Ordering::Acquire) & 1 != 0;
        let same_image = query_region(saved.table).map(|r| {
            r.State == MEM_COMMIT
                && r.Type == MEM_IMAGE
                && r.AllocationBase as usize == saved.module
        });
        if unloaded || same_image == Some(false) {
            // An unload invalidates this installation even if a new image has
            // since reused exactly the same base address. Never restore old
            // page permissions or pointers into the replacement image.
            for i in 0..3 {
                saved.patched[i].store(false, Ordering::Release);
                saved.protection[i].store(0, Ordering::Release);
            }
        } else if held && same_image == Some(true) {
            for m in Unknown::ALL.into_iter().rev() {
                if saved.patched[m.slot()].load(Ordering::Acquire)
                    || saved.protection[m.slot()].load(Ordering::Acquire) != 0
                {
                    // SAFETY: the module reference taken above keeps the vtable mapped.
                    unsafe {
                        exchange(saved, index, m, false);
                    }
                }
            }
        }
        if held {
            // SAFETY: releases the module reference taken above, once.
            unsafe {
                FreeLibrary(module);
            }
        }
        if pending(saved) {
            unrestored += 1;
        }
        if !saved.published.load(Ordering::Acquire) && !pending(saved) {
            SLOTS[index].store(std::ptr::null_mut(), Ordering::Release);
            MODULES[index].store(0, Ordering::Release);
            *entry = None;
        }
    }
    unrestored
}

#[cfg(test)]
mod tests {
    unsafe fn install(
        rt: &std::sync::Arc<crate::runtime::HookRuntime>,
        object: usize,
    ) -> Result<bool, &'static str> {
        // SAFETY: forwarded from this function's own contract.
        unsafe { super::install(rt, object) }.map(|installation| installation.newly_installed)
    }

    use super::*;
    use crate::com::IID_IUNKNOWN;
    use shirusu_agent::Agent;
    use shirusu_transport::{context::HostMonitor, layout::RingConfig, slot::SlotPool};
    use std::cell::UnsafeCell;
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};

    #[repr(C)]
    struct Vtable {
        query: Qi,
        add: Ref,
        release: Ref,
        extra: Ref,
    }
    struct SharedTable(UnsafeCell<Vtable>);
    // Native vtable slots are accessed through AtomicUsize after initialization.
    // SAFETY: the table is only read and written through AtomicUsize after initialisation.
    unsafe impl Sync for SharedTable {}
    // A dedicated image section lets the test make only this fixture read-only.
    #[link_section = ".shcom"]
    static TABLE: SharedTable = SharedTable(UnsafeCell::new(Vtable {
        query: native_query,
        add: native_add,
        release: native_release,
        extra: native_extra,
    }));
    #[repr(C)]
    struct Object {
        table: usize,
        refs: AtomicU32,
        incoming_error: AtomicU32,
    }
    extern "C-unwind" {
        fn shirusu_test_raise_seh() -> u64;
        fn shirusu_test_catch_seh(
            invoke: unsafe extern "C-unwind" fn(*mut c_void) -> u64,
            context: *mut c_void,
        ) -> u32;
    }
    static RAISE_ORIGINAL: AtomicBool = AtomicBool::new(false);
    static BLOCK_ORIGINAL: AtomicBool = AtomicBool::new(false);
    static ENTERED_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static RELEASE_ORIGINAL: AtomicBool = AtomicBool::new(false);
    unsafe extern "system-unwind" fn native_add(this: *mut c_void) -> u32 {
        // SAFETY: the fixture passes `this` = a live Object.
        let object = unsafe { &*this.cast::<Object>() };
        object
            .incoming_error
            // SAFETY: GetLastError has no preconditions.
            .store(unsafe { GetLastError() }, Ordering::Relaxed);
        if RAISE_ORIGINAL.load(Ordering::Acquire) {
            // SAFETY: the test's SEH helper raises an exception that the caller's catch frame handles.
            unsafe {
                shirusu_test_raise_seh();
            }
        }
        let count = object.refs.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        if BLOCK_ORIGINAL.load(Ordering::Acquire) {
            ENTERED_ORIGINAL.fetch_add(1, Ordering::Release);
            while !RELEASE_ORIGINAL.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        // SAFETY: SetLastError has no preconditions.
        unsafe {
            SetLastError(0x4444);
        }
        count
    }
    unsafe extern "system-unwind" fn native_release(this: *mut c_void) -> u32 {
        // SAFETY: the fixture passes `this` = a live Object.
        let object = unsafe { &*this.cast::<Object>() };
        let count = object.refs.fetch_sub(1, Ordering::SeqCst).wrapping_sub(1);
        // SAFETY: SetLastError has no preconditions.
        unsafe {
            SetLastError(0x5555);
        }
        count
    }
    unsafe extern "system-unwind" fn native_query(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        // SAFETY: callers pass a valid IID pointer.
        if unsafe { *iid } != IID_IUNKNOWN {
            // SAFETY: callers pass a writable `out` pointer.
            unsafe {
                *out = std::ptr::null_mut();
                SetLastError(0x6666);
            }
            return 0x80004002u32 as i32;
        }
        // SAFETY: `this` is a live Object (caller's contract).
        unsafe {
            native_add(this);
            *out = this;
            SetLastError(0x7777);
        }
        0
    }
    unsafe extern "system-unwind" fn native_extra(_: *mut c_void) -> u32 {
        0xfeedbeef
    }
    unsafe extern "system-unwind" fn third_party_add(this: *mut c_void) -> u32 {
        // SAFETY: `this` is a live Object (caller's contract).
        unsafe { native_add(this) }
    }
    fn runtime() -> Arc<HookRuntime> {
        Arc::new(HookRuntime::new(
            Arc::new(Agent::new(
                Arc::new(SlotPool::new(8)),
                RingConfig::new(8192, 1024 * 1024),
                Arc::new(HostMonitor::always_alive()),
            )),
            Vec::new(),
            None,
        ))
    }
    fn object(rt: &Arc<HookRuntime>) -> Arc<Object> {
        let object = Arc::new(Object {
            table: TABLE.0.get() as usize,
            refs: AtomicU32::new(1),
            incoming_error: AtomicU32::new(0),
        });
        rt.with_com_tracker(|t| {
            t.note_factory_result(Arc::as_ptr(&object) as u64, IID_IUNKNOWN, "fixture");
        });
        object
    }
    fn method(object: &Object, index: usize) -> usize {
        // SAFETY: the fixture vtable has 4 aligned slots and `index` is below 4; slots are only accessed atomically.
        unsafe { AtomicUsize::from_ptr((object.table as *mut usize).add(index)) }
            .load(Ordering::Acquire)
    }
    unsafe fn call_ref(object: &Object, index: usize) -> u32 {
        // SAFETY: `index` names an AddRef/Release slot (Ref signature) of the live object's vtable.
        unsafe {
            std::mem::transmute::<usize, Ref>(method(object, index))(
                object as *const Object as *mut c_void,
            )
        }
    }

    #[test]
    fn image_vtable_identity_concurrency_rollback_and_late_calls() {
        // Global vtable patching and injected failures must not share a process
        // with unrelated Rust test threads or other hook installation tests.
        const CHILD: &str = "SHIRUSU_SHARED_IUNKNOWN_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shared_com::tests::image_vtable_identity_concurrency_rollback_and_late_calls",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "shared IUnknown child failed: {status}");
            return;
        }
        assert!(initialize());
        let mut observed_unload = false;
        for name in ["winmm.dll", "msacm32.dll", "dinput8.dll", "version.dll"] {
            let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
            // SAFETY: the name is a NUL-terminated UTF-16 string that outlives the call.
            if !unsafe {
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(wide.as_ptr())
            }
            .is_null()
            {
                continue;
            }
            let module =
                // SAFETY: `wide` is a NUL-terminated UTF-16 path.
                unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
            if module.is_null() {
                continue;
            }
            let index = LIMIT - 1;
            MODULES[index].store(module as usize, Ordering::Release);
            let epoch = (MODULE_EPOCHS[index].load(Ordering::Acquire) | 1)
                .checked_add(1)
                .unwrap();
            MODULE_EPOCHS[index].store(epoch, Ordering::Release);
            // SAFETY: `module` was loaded above and is freed once.
            assert_ne!(unsafe { FreeLibrary(module) }, 0);
            observed_unload = MODULE_EPOCHS[index].load(Ordering::Acquire) & 1 != 0;
            MODULES[index].store(0, Ordering::Release);
            if observed_unload {
                break;
            }
        }
        assert!(
            observed_unload,
            "real DLL unload notification was not observed"
        );
        let table = TABLE.0.get() as usize;
        let mut initial_protection = 0;
        assert_ne!(
            // SAFETY: `table` is the fixture's static table and `initial_protection` a writable local.
            unsafe {
                VirtualProtect(
                    table as *const c_void,
                    std::mem::size_of::<Vtable>(),
                    PAGE_READONLY,
                    &mut initial_protection,
                )
            },
            0
        );
        let rt = runtime();
        let object = object(&rt);
        let ptr = Arc::as_ptr(&object) as usize;
        let original = [
            method(&object, 0),
            method(&object, 1),
            method(&object, 2),
            method(&object, 3),
        ];
        let heap_table = Box::new(original);
        let heap_object = Object {
            table: heap_table.as_ptr() as usize,
            refs: AtomicU32::new(1),
            incoming_error: AtomicU32::new(0),
        };
        assert_eq!(
            // SAFETY: the object is live; install only reads it and rejects the heap vtable.
            unsafe { install(&rt, (&raw const heap_object).expose_provenance()) },
            Err("vtable-not-image-data")
        );
        assert_eq!(*heap_table, original);
        let stop = Arc::new(AtomicBool::new(false));
        let iterations = Arc::new(AtomicUsize::new(0));
        let reader = {
            let object = Arc::clone(&object);
            let stop = Arc::clone(&stop);
            let iterations = Arc::clone(&iterations);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
                    assert_eq!(unsafe { call_ref(&object, 1) }, 2);
                    // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
                    assert_eq!(unsafe { call_ref(&object, 2) }, 1);
                    iterations.fetch_add(1, Ordering::Release);
                }
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while iterations.load(Ordering::Acquire) < 100 {
            assert!(
                std::time::Instant::now() < deadline,
                "concurrent reader did not start"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
            unsafe { crate::engine::intercept_object(&rt, ptr, IID_IUNKNOWN) },
            crate::engine::ComInterceptOutcome::IUnknownIntercepted
        );
        assert!(rt.with_com_tracker(|t| t.lookup(ptr as u64).unwrap().iunknown_intercepted));
        stop.store(true, Ordering::Release);
        reader.join().unwrap();
        // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
        assert_eq!(unsafe { install(&rt, ptr) }, Ok(false));
        assert_eq!(object.table, table, "object identity/vptr must not change");
        assert_eq!(
            method(&object, 3),
            original[3],
            "derived slots must remain untouched"
        );
        assert_eq!(query_region(table).unwrap().Protect, PAGE_READONLY);

        // SAFETY: slot 1 holds a Ref-compatible pointer (the installed wrapper).
        let cached_add: Ref = unsafe { std::mem::transmute(method(&object, 1)) };
        // SAFETY: SetLastError has no preconditions.
        unsafe {
            SetLastError(0x1234abcd);
        }
        // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
        let count = unsafe { cached_add(ptr as *mut c_void) };
        // SAFETY: GetLastError has no preconditions.
        let error = unsafe { GetLastError() };
        assert_eq!(count, 2);
        assert_eq!(error, 0x4444);
        assert_eq!(object.incoming_error.load(Ordering::Relaxed), 0x1234abcd);
        // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
        assert_eq!(unsafe { call_ref(&object, 2) }, 1);

        // SAFETY: slot 0 holds a Qi-compatible pointer (the installed wrapper).
        let query: Qi = unsafe { std::mem::transmute(method(&object, 0)) };
        let mut queried = std::ptr::null_mut();
        assert_eq!(
            // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
            unsafe { query(ptr as *mut c_void, &IID_IUNKNOWN, &mut queried) },
            0
        );
        // SAFETY: GetLastError has no preconditions.
        let error = unsafe { GetLastError() };
        assert_eq!(queried as usize, ptr);
        assert_eq!(error, 0x7777);
        assert!(rt.with_com_tracker(|t| t.lookup(ptr as u64).unwrap().identity_confirmed));
        // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
        assert_eq!(unsafe { call_ref(&object, 2) }, 1);

        unsafe extern "C-unwind" fn invoke_add(context: *mut c_void) -> u64 {
            // SAFETY: the SEH helper passes the live fixture object as `context`.
            unsafe { call_ref(&*context.cast::<Object>(), 1) as u64 }
        }
        let references = Arc::strong_count(&rt);
        RAISE_ORIGINAL.store(true, Ordering::Release);
        // SAFETY: `invoke_add` matches the helper's callback ABI and `ptr` stays alive.
        let exception = unsafe { shirusu_test_catch_seh(invoke_add, ptr as *mut c_void) };
        RAISE_ORIGINAL.store(false, Ordering::Release);
        assert_eq!(exception, 0xe0424242);
        assert_eq!(
            Arc::strong_count(&rt),
            references,
            "SEH must release recording ownership"
        );
        assert!(rt.wait_in_flight_drained(std::time::Duration::ZERO));
        assert!(!crate::runtime::ReentrancyGuard::is_active());
        assert_eq!(crate::runtime::current_parent_call(), 0);

        // Other live objects sharing the same table are passed through when
        // their provenance is unknown, rather than assigned a fabricated IID.
        let other = Object {
            table,
            refs: AtomicU32::new(1),
            incoming_error: AtomicU32::new(0),
        };
        // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
        assert_eq!(unsafe { call_ref(&other, 1) }, 2);
        assert!(rt.with_com_tracker(|t| t.lookup((&raw const other).addr() as u64).is_none()));
        // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
        assert_eq!(unsafe { call_ref(&other, 2) }, 1);

        // Reference counts are ULONG, not HRESULT or pointer-sized returns.
        object.refs.store(0x80000000, Ordering::SeqCst);
        // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
        assert_eq!(unsafe { call_ref(&object, 1) }, 0x80000001);
        assert_eq!(
            rt.with_com_tracker(|t| t.lookup(ptr as u64).unwrap().refcount),
            Some(0x80000001)
        );
        let tid = crate::thread_id::current() as u64;
        let base = rt
            .agent()
            .thread_region(tid)
            .expect("recorded main-thread Ring");
        // SAFETY: the only reader of this Ring (SPSC).
        let mut reader = unsafe {
            shirusu_transport::ring::RingReader::new(base, RingConfig::new(8192, 1024 * 1024))
        }
        .unwrap();
        let batch = reader.drain_all();
        let types =
            shirusu_trace::schema::schema_types(&crate::com::com_method_leave_schema()).unwrap();
        assert!(
            batch
                .events
                .iter()
                .filter(|e| e.descriptor.kind
                    == shirusu_transport::descriptor::EventKind::CallLeave as u16
                    && e.descriptor.schema_id == crate::com::COM_METHOD_LEAVE_SCHEMA_ID)
                .any(|e| {
                    let values = shirusu_trace::compact::decode_values(&types, &e.payload).unwrap();
                    values[2].value == shirusu_trace::schema::TypedValue::U32(0x80000001)
                        && values[3].value
                            == shirusu_trace::schema::TypedValue::Str("refcount".into())
                }),
            "native ULONG result must carry the explicit refcount kind in the Trace"
        );
        object.refs.store(1, Ordering::SeqCst);

        let barrier = Arc::new(std::sync::Barrier::new(5));
        let mut threads = Vec::new();
        for _ in 0..4 {
            // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
            unsafe {
                call_ref(&object, 1);
            } // transfer one owned COM reference
            let object = Arc::clone(&object);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..2000 {
                    // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
                    let count = unsafe { call_ref(&object, 1) };
                    // SAFETY: GetLastError has no preconditions.
                    let error = unsafe { GetLastError() };
                    assert!(count > 1);
                    assert_eq!(error, 0x4444);
                    // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
                    let count = unsafe { call_ref(&object, 2) };
                    // SAFETY: GetLastError has no preconditions.
                    let error = unsafe { GetLastError() };
                    assert!(count > 0);
                    assert_eq!(error, 0x5555);
                }
                // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
                unsafe {
                    call_ref(&object, 2);
                }
            }));
        }
        BLOCK_ORIGINAL.store(true, Ordering::Release);
        barrier.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ENTERED_ORIGINAL.load(Ordering::Acquire) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "original method was not reached"
            );
            std::thread::yield_now();
        }
        rt.shutdown_recording();
        assert!(!rt.wait_in_flight_drained(std::time::Duration::ZERO));
        assert_eq!(
            restore_all(),
            0,
            "restore while cached wrappers are in flight"
        );
        RELEASE_ORIGINAL.store(true, Ordering::Release);
        BLOCK_ORIGINAL.store(false, Ordering::Release);
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(object.refs.load(Ordering::SeqCst), 1);
        assert_eq!(object.table, table);
        for (i, expected) in original.iter().enumerate() {
            assert_eq!(method(&object, i), *expected);
        }
        assert_eq!(
            // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
            unsafe { cached_add(ptr as *mut c_void) },
            2,
            "late cached wrapper must call the original"
        );
        // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
        assert_eq!(unsafe { call_ref(&object, 2) }, 1);
        // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
        assert_eq!(unsafe { install(&rt, ptr) }, Err("session-closing"));

        let rt = runtime();
        let object = self::object(&rt);
        let ptr = Arc::as_ptr(&object) as usize;
        // Repeated unpublished failures do not consume the permanent quota.
        for _ in 0..(LIMIT + 8) {
            FAIL_WRITABLE.store(1, Ordering::Relaxed);
            assert_eq!(
                // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
                unsafe { install(&rt, ptr) },
                Err("iunknown-installation-incomplete")
            );
        }
        assert!(
            REGISTRY
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.is_some())
                .count()
                < 3
        );
        // Fail the original protection restore and its immediate rollback.
        FAIL_RESTORE.store(2, Ordering::Relaxed);
        assert_eq!(
            // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
            unsafe { install(&rt, ptr) },
            Err("iunknown-installation-incomplete")
        );
        assert_eq!(method(&object, 0), original[0]);
        assert_eq!(query_region(table).unwrap().Protect, PAGE_READWRITE);
        assert!(REGISTRY
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|s| pending(s)));
        rt.shutdown_recording();
        assert_eq!(restore_all(), 0);
        assert_eq!(query_region(table).unwrap().Protect, PAGE_READONLY);

        // A second session for the same table and originals reuses the
        // restored wrapper identity instead of spending another of the LIMIT
        // slots (2026-11-14 review P3-6). Restoration respects a later
        // third-party slot update instead of overwriting it.
        let used = REGISTRY.lock().unwrap().iter().flatten().count();
        let rt = runtime();
        let object = self::object(&rt);
        let ptr = Arc::as_ptr(&object) as usize;
        // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
        assert_eq!(unsafe { install(&rt, ptr) }, Ok(true));
        assert_eq!(method(&object, 1), cached_add as usize);
        assert_eq!(REGISTRY.lock().unwrap().iter().flatten().count(), used);
        let address = table + std::mem::size_of::<usize>();
        let mut old = 0;
        assert!(make_writable(address, &mut old));
        // SAFETY: the fixture table is an aligned usize array in a page made writable above.
        unsafe { AtomicUsize::from_ptr(address as *mut usize) }
            .store(third_party_add as Ref as usize, Ordering::Release);
        assert!(restore_page(address, old));
        rt.shutdown_recording();
        assert_eq!(restore_all(), 0);
        assert_eq!(method(&object, 1), third_party_add as Ref as usize);
        assert!(make_writable(address, &mut old));
        // SAFETY: the fixture table is an aligned usize array in a page made writable above.
        unsafe { AtomicUsize::from_ptr(address as *mut usize) }
            .store(original[1], Ordering::Release);
        assert!(restore_page(address, old));
        // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
        assert_eq!(unsafe { cached_add(ptr as *mut c_void) }, 2);
        // SAFETY: the fixture object's vtable slots have the Ref signature and the object outlives the call.
        assert_eq!(unsafe { call_ref(&object, 2) }, 1);
        // Model an unload followed by a new image at the same address. The
        // observer's generation invalidates ownership even when VirtualQuery
        // still reports the same image base. Old pending protection is not
        // applied to the replacement mapping.
        let rt = runtime();
        let object = self::object(&rt);
        assert_eq!(
            // SAFETY: `ptr` points to the live fixture object, whose first word is its vtable.
            unsafe { install(&rt, Arc::as_ptr(&object) as usize) },
            Ok(true)
        );
        let replacements = [method(&object, 0), method(&object, 1), method(&object, 2)];
        let owner = Arc::downgrade(&rt);
        let index = REGISTRY
            .lock()
            .unwrap()
            .iter()
            .position(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|entry| entry.owner.ptr_eq(&owner))
            })
            .unwrap();
        assert!(make_writable(table, &mut old));
        slot(index).protection[0].store(PAGE_READONLY, Ordering::Release);
        let notification = DllNotification {
            flags: 0,
            full_name: std::ptr::null(),
            base_name: std::ptr::null(),
            base: query_region(table).unwrap().AllocationBase,
            size: 0,
        };
        // SAFETY: `notification` is a valid record for the duration of the call.
        unsafe {
            dll_notification(2, &notification, std::ptr::null_mut());
        }
        rt.shutdown_recording();
        assert_eq!(restore_all(), 0);
        assert_eq!(query_region(table).unwrap().Protect, PAGE_READWRITE);
        for (i, expected) in replacements.iter().enumerate() {
            assert_eq!(method(&object, i), *expected);
        }
        // The notification above was a model injection; restore the actual test
        // table directly before leaving the child process.
        for (i, original) in original[..3].iter().enumerate() {
            // SAFETY: the fixture table is an aligned usize array in a page made writable above.
            unsafe { AtomicUsize::from_ptr((table as *mut usize).add(i)) }
                .store(*original, Ordering::Release);
        }
        assert!(restore_page(table, initial_protection));
    }
}
