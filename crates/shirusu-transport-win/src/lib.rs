//! shirusu-transport-win: Windows バックエンド。
//! - 名前付きでない File Mapping を Host が作り、`DuplicateHandle` で対象プロセスへ渡す前提の API。
//! - Event Object: `data_ready` はプロセス単位・auto-reset、`space_available` は Ring 単位。
//! - Host 待機は 2ms 再確認付き（transport の ReCheckPolicy と対応）。
//!
//! Windows 以外では同じ API 形のスタブとしてコンパイルでき、プロセス間機能は Unsupported を返す。

use shirusu_transport::notify::ReCheckPolicy;
#[cfg(windows)]
use shirusu_transport::Backing;
use shirusu_transport::Shm;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// L23: ロード済みモジュールの列挙・署名検証・API Set 契約の解決。
pub mod modules;
pub mod system;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinError {
    Unsupported,
    OsError {
        api: &'static str,
        code: u32,
    },
    /// The remote thread may still be using its parameter. Do not free it.
    RemoteCallPending {
        wait_status: u32,
    },
    InvalidArgument(&'static str),
}
impl std::fmt::Display for WinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "not supported on this platform"),
            Self::OsError { api, code } => write!(f, "{api} failed with OS error {code}"),
            Self::RemoteCallPending { wait_status } => write!(
                f,
                "remote call did not finish (wait status {wait_status}); parameter memory retained"
            ),
            Self::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
        }
    }
}
impl std::error::Error for WinError {}
pub type Result<T> = std::result::Result<T, WinError>;

/// このプロセスが所有するハンドル。Drop で一度だけ閉じる (Windows 以外は
/// 何もしない)。複製も Copy もできないので、二重に閉じることも、閉じた
/// 後に使うこともない (レビュー P1-4)。
#[derive(Debug, PartialEq, Eq)]
pub struct OwnedHandle {
    raw: usize,
}
// SAFETY: カーネルオブジェクトのハンドルはどのスレッドからも使え、閉じられる。
unsafe impl Send for OwnedHandle {}
// SAFETY: &OwnedHandle で行う操作 (待機・照会・複製) はスレッドセーフ。
unsafe impl Sync for OwnedHandle {}
impl OwnedHandle {
    /// 受け取ったハンドル値の所有権を引き受ける。0 は None。
    ///
    /// # Safety
    /// `raw` はこのプロセスで有効なハンドルで、ほかの誰も閉じないこと
    /// (継承や DuplicateHandle でこのプロセスへ渡された値など)。
    pub unsafe fn from_raw(raw: usize) -> Option<Self> {
        (raw != 0).then_some(Self { raw })
    }
    pub fn borrow(&self) -> BorrowedHandle<'_> {
        BorrowedHandle {
            raw: self.raw,
            _owner: std::marker::PhantomData,
        }
    }
}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            // SAFETY: 所有するハンドルを一度だけ閉じる。
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.raw as _) };
        }
    }
}

/// 借りたハンドル。持ち主 ([`OwnedHandle`] など) より長く生きられない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowedHandle<'a> {
    raw: usize,
    _owner: std::marker::PhantomData<&'a OwnedHandle>,
}
impl BorrowedHandle<'_> {
    /// 数値 (継承ハンドルをコマンドラインで子へ伝えるときなど)。
    pub fn raw(self) -> usize {
        self.raw
    }
}

/// 別のプロセスで有効なハンドル値。このプロセスでは使えない (閉じるには
/// [`close_remote_handle`])。受け取った側は [`OwnedHandle::from_raw`] で
/// 引き受ける。
///
/// HANDLE 値はプロセスごとの Handle Table で解釈されるので、値は複製先の
/// プロセス ID と組で持つ (レビュー 2026-11-16 P2)。閉じる操作は持っている
/// ID のプロセスにしか向かず、別プロセスの値と取り違える引数を書けない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemoteHandle {
    raw: usize,
    target_pid: u32,
}
impl RemoteHandle {
    pub fn raw(self) -> usize {
        self.raw
    }
    /// この値を解釈するプロセス。
    pub fn target_pid(self) -> u32 {
        self.target_pid
    }
    /// `pid` のプロセス向けの値なら数値を返す (Bootstrap など、特定の
    /// プロセスへ数値で渡す場所はここを通す)。
    pub fn raw_for(self, pid: u32) -> Option<usize> {
        (self.target_pid == pid).then_some(self.raw)
    }
}

/// Peak memory counters of a process (review 2026-11-14 V3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessPeakMemory {
    pub peak_working_set: u64,
    /// Peak private commit (`PeakPagefileUsage`).
    pub peak_private: u64,
}

/// 起動したプロセスのハンドル一式（CREATE_SUSPENDED 起動用）。
#[derive(Debug)]
pub struct SpawnedProcess {
    pub process: OwnedHandle,
    pub thread: OwnedHandle,
    pub pid: u32,
}

// Win32 waits accept integer milliseconds. Round positive fractional values up:
// truncation turns the final sub-millisecond recheck slice into a busy poll.
// Reserve INFINITE (u32::MAX); a finite Duration must remain a finite wait.
#[cfg(any(windows, test))]
fn wait_timeout_millis(timeout: Duration) -> u32 {
    timeout
        .as_nanos()
        .div_ceil(1_000_000)
        .min(u128::from(u32::MAX - 1)) as u32
}

/// これより短い待機は高分解能タイマーで待つ (P2-14)。既定のタイマー
/// 周期は約 15.6ms。
#[cfg(windows)]
const PRECISE_WAIT_BELOW_MS: u32 = 16;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, SetHandleInformation, DUPLICATE_SAME_ACCESS,
        HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcess, SetEvent, WaitForSingleObject,
        PROCESS_DUP_HANDLE,
    };

    pub(super) fn last_err(api: &'static str) -> WinError {
        WinError::OsError {
            api,
            // SAFETY: GetLastError has no preconditions.
            code: unsafe { GetLastError() },
        }
    }
    pub(super) fn h(v: BorrowedHandle<'_>) -> HANDLE {
        v.raw() as HANDLE
    }
    /// 作ったばかりのハンドルを所有する (null でないことは呼び出し側が確認済み)。
    pub(super) fn owned(handle: HANDLE) -> OwnedHandle {
        OwnedHandle {
            raw: handle as usize,
        }
    }
    /// このプロセスの疑似ハンドル (閉じる必要も、閉じられることもない)。
    #[cfg(test)]
    pub(super) fn current_process() -> BorrowedHandle<'static> {
        BorrowedHandle {
            // SAFETY: no preconditions.
            raw: unsafe { GetCurrentProcess() } as usize,
            _owner: std::marker::PhantomData,
        }
    }
    /// `handle` を対象プロセスへ DuplicateHandle で渡す（Mapping と Event の共通）。返した値は
    /// 対象プロセス側のハンドル値。
    fn duplicate_to_process(handle: BorrowedHandle<'_>, target_pid: u32) -> Result<RemoteHandle> {
        // SAFETY: OpenProcess takes only values; failure returns null, checked below.
        let target = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, target_pid) };
        if target.is_null() {
            return Err(last_err("OpenProcess"));
        }
        let mut dup: HANDLE = std::ptr::null_mut();
        // SAFETY: `handle` and `target` are open handles and `dup` is a writable local.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h(handle),
                target,
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        // SAFETY: `target` was opened above, is not used after this, and is closed once.
        unsafe { CloseHandle(target) };
        if ok == 0 {
            return Err(last_err("DuplicateHandle"));
        }
        Ok(remote(dup, target_pid))
    }

    /// `handle` を継承可能にする（CreateProcess で子へ渡す）。`what` は失敗時の名前。
    fn make_inheritable(handle: BorrowedHandle<'_>, what: &'static str) -> Result<()> {
        // SAFETY: `handle` is an open handle its owner keeps open for this call.
        let ok = unsafe { SetHandleInformation(h(handle), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        if ok == 0 {
            return Err(last_err(what));
        }
        Ok(())
    }

    pub(super) fn remote(handle: HANDLE, target_pid: u32) -> RemoteHandle {
        RemoteHandle {
            raw: handle as usize,
            target_pid,
        }
    }

    /// 現在のユーザーの SID だけに GENERIC_ALL を許可する DACL 付き
    /// SECURITY_ATTRIBUTES を作る。バッファは構造体が保持し、呼び出し元が
    /// CreateFileMappingW を呼ぶ間だけ有効であればよい。
    /// `acl`/`sd` は構築後にフィールドとして読まれないが、解放してはならない:
    /// `sa.lpSecurityDescriptor` が `sd` を指し、`sd` 内の DACL が `acl` を
    /// 指しているため、この構造体がバッファの生存を保証している。
    #[allow(dead_code)]
    struct RestrictedSecurityAttributes {
        /// TOKEN_USER のコピー。SID は ACE へ複写済みだが念のため保持する。
        _token_info: Vec<u8>,
        acl: Vec<u8>,
        sd: Vec<u8>,
        sa: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
    }

    fn restricted_security_attributes() -> Result<RestrictedSecurityAttributes> {
        // SECURITY_DESCRIPTOR_REVISION (= 1)。Win32 では Win32::System::SystemServices
        // にある定数だが値は固定のため、feature 追加を避けてここで定義する。
        const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
        use windows_sys::Win32::Foundation::GENERIC_ALL;
        use windows_sys::Win32::Security::{
            AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
            InitializeSecurityDescriptor, SetSecurityDescriptorDacl, TokenUser, ACCESS_ALLOWED_ACE,
            ACL, ACL_REVISION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
            TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::OpenProcessToken;
        // 1. プロセストークンからユーザー SID を取得する。
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo handle needs no closing and `token` is a writable local.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_err("OpenProcessToken"));
        }
        let mut needed: u32 = 0;
        // SAFETY: a null buffer of length 0 only asks for the size (written to `needed`).
        unsafe {
            // 必要サイズの問い合わせ（ERROR_INSUFFICIENT_BUFFER を期待する）。
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        }
        let mut token_info = vec![0u8; needed as usize];
        // SAFETY: `token_info` has room for the `needed` bytes reported above.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            let e = last_err("GetTokenInformation");
            // SAFETY: `token` was opened above and is closed once on this path.
            unsafe { CloseHandle(token) };
            return Err(e);
        }
        // SAFETY: `token` was opened above and is closed once on this path.
        unsafe { CloseHandle(token) };
        // SAFETY: GetTokenInformation filled `token_info` with a TOKEN_USER (its SID points inside the buffer, which lives to the end of this function).
        let user = unsafe { &*(token_info.as_ptr() as *const TOKEN_USER) };
        let sid = user.User.Sid;
        // 2. ACL を構築する（ユーザー SID に GENERIC_ALL の ACE 1 本だけ）。
        // SAFETY: `sid` points to a valid SID inside `token_info`.
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        let acl_len = std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>()
            - std::mem::size_of::<u32>()
            + sid_len;
        let mut acl = vec![0u8; acl_len];
        let acl_ptr = acl.as_mut_ptr() as *mut ACL;
        // SAFETY: `acl` is a writable buffer of `acl_len` bytes.
        if unsafe { InitializeAcl(acl_ptr, acl_len as u32, ACL_REVISION) } == 0 {
            return Err(last_err("InitializeAcl"));
        }
        // SAFETY: `acl_ptr` was initialised with room for one ACE of this SID length.
        if unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, sid) } == 0 {
            return Err(last_err("AddAccessAllowedAce"));
        }
        // 3. セキュリティ記述子に DACL を設定する。
        let mut sd = vec![0u8; std::mem::size_of::<SECURITY_DESCRIPTOR>()];
        let sd_ptr = sd.as_mut_ptr() as PSECURITY_DESCRIPTOR;
        // SAFETY: `sd` is a writable buffer of SECURITY_DESCRIPTOR size.
        if unsafe { InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) } == 0 {
            return Err(last_err("InitializeSecurityDescriptor"));
        }
        // SAFETY: `sd_ptr` is initialised and `acl_ptr` outlives it (both are kept in the returned struct).
        if unsafe { SetSecurityDescriptorDacl(sd_ptr, 1, acl_ptr, 0) } == 0 {
            return Err(last_err("SetSecurityDescriptorDacl"));
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd_ptr,
            bInheritHandle: 0,
        };
        Ok(RestrictedSecurityAttributes {
            _token_info: token_info,
            acl,
            sd,
            sa,
        })
    }

    /// File Mapping のサイズを CreateFileMappingW の (high, low) DWORD
    /// 対へ分ける。`usize` のまま `>> 32` すると、32bit ターゲットでは
    /// シフト量が型の幅以上になり算術オーバーフローとしてコンパイル
    /// エラーになる (レビュー #4 の #1: i686 ビルド失敗の原因)。
    /// u64 へ広げてから分ける。
    fn split_mapping_size(len: usize) -> (u32, u32) {
        let size = len as u64;
        ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32)
    }

    /// Mapping のビュー。最後の [`Shm`] が消えたときに解放する (セクションは
    /// ビューが残る限り OS が保つので、ハンドルを先に閉じてもよい)。
    struct MappedView {
        view: std::ptr::NonNull<u8>,
        len: usize,
    }
    // SAFETY: ビューのアドレスは全スレッドで同じで、中身へは Shm 経由でしか触れない。
    unsafe impl Send for MappedView {}
    // SAFETY: 同上。
    unsafe impl Sync for MappedView {}
    // SAFETY: MapViewOfFile のビューは割り当て粒度 (64 KiB) 境界にあり、
    // UnmapViewOfFile (Drop) まで len バイトが読み書きできる。参照は作らない。
    unsafe impl Backing for MappedView {
        fn base(&self) -> std::ptr::NonNull<u8> {
            self.view
        }
        fn byte_len(&self) -> usize {
            self.len
        }
    }
    impl Drop for MappedView {
        fn drop(&mut self) {
            // SAFETY: MapViewOfFile が返したビューを一度だけ解放する。
            unsafe {
                UnmapViewOfFile(
                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.view.as_ptr() as *mut c_void,
                    },
                );
            }
        }
    }
    /// ビューを Shm にする。Control が入らない大きさなら解放して Err。
    fn map_view(handle: HANDLE, len: usize, api: &'static str) -> Result<Shm> {
        // SAFETY: handle は呼び出し側が検証した File Mapping ハンドル。
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        let Some(view) = std::ptr::NonNull::new(view.Value as *mut u8) else {
            return Err(last_err(api));
        };
        Shm::new(std::sync::Arc::new(MappedView { view, len }))
            .map_err(|_| WinError::InvalidArgument("len"))
    }

    pub struct SharedMapping {
        handle: OwnedHandle,
        shm: Shm,
    }
    impl SharedMapping {
        pub fn create(len: usize) -> Result<Self> {
            if len < shirusu_transport::layout::CONTROL_SIZE {
                return Err(WinError::InvalidArgument("len"));
            }
            let (size_high, size_low) = split_mapping_size(len);
            // SAFETY: an unnamed page-file-backed section with default security; failure returns null, checked below.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    size_high,
                    size_low,
                    std::ptr::null(), // 名前付きにしない
                )
            };
            if handle.is_null() {
                return Err(last_err("CreateFileMappingW"));
            }
            Self::from_owned(owned(handle), len)
        }
        /// アクセス制御付きで作る。仕様: 共有メモリのセクションには、Host と対象
        /// プロセス以外から開けないアクセス制御を付ける。DACL は現在のユーザーの
        /// SID にだけ GENERIC_ALL を許可し、それ以外には何も許可しない（Host と
        /// 対象プロセスは同じユーザーで動く前提）。Event 等のハンドル受け渡しは
        /// DuplicateHandle 経由のため、この DACL では妨げない。
        pub fn create_restricted(len: usize) -> Result<Self> {
            if len < shirusu_transport::layout::CONTROL_SIZE {
                return Err(WinError::InvalidArgument("len"));
            }
            let restricted = restricted_security_attributes()?;
            let (size_high, size_low) = split_mapping_size(len);
            // SAFETY: an unnamed section; `restricted` keeps its descriptor and ACL alive for the call.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    &restricted.sa,
                    PAGE_READWRITE,
                    size_high,
                    size_low,
                    std::ptr::null(), // 名前付きにしない
                )
            };
            if handle.is_null() {
                return Err(last_err("CreateFileMappingW(restricted)"));
            }
            Self::from_owned(owned(handle), len)
        }
        /// 所有するハンドルから `len` バイトのビューを作る。File Mapping で
        /// ない、または `len` より小さいハンドルは MapViewOfFile が拒む。
        /// 失敗したらハンドルは閉じる。
        pub fn from_owned(handle: OwnedHandle, len: usize) -> Result<Self> {
            let shm = map_view(h(handle.borrow()), len, "MapViewOfFile")?;
            Ok(Self { handle, shm })
        }
        pub fn as_handle(&self) -> BorrowedHandle<'_> {
            self.handle.borrow()
        }
        /// 対象プロセスへ DuplicateHandle で渡す。
        pub fn duplicate_to_process(&self, target_pid: u32) -> Result<RemoteHandle> {
            duplicate_to_process(self.handle.borrow(), target_pid)
        }
        /// 継承可能なハンドルにする（CreateProcess で子へ渡す E2E 用）。
        pub fn make_inheritable(&self) -> Result<()> {
            make_inheritable(self.handle.borrow(), "SetHandleInformation")
        }
        /// ビュー。複製はビューの参照を増やすだけで、Mapping を落としても
        /// 複製が生きている間はビューが残る。
        pub fn shm(&self) -> &Shm {
            &self.shm
        }
        pub fn len(&self) -> usize {
            self.shm.len()
        }
        pub fn is_empty(&self) -> bool {
            self.shm.is_empty()
        }
    }
    /// カーネルのイベント操作（SetEvent / WaitForSingleObject）はスレッド
    /// セーフなため、`&EventObject` を複数スレッドで共有できる。
    pub struct EventObject {
        handle: OwnedHandle,
        auto_reset: bool,
    }
    impl EventObject {
        pub fn new(auto_reset: bool, initially_signaled: bool) -> Result<Self> {
            // SAFETY: an anonymous event with default security; failure returns null, checked below.
            let handle = unsafe {
                CreateEventW(
                    std::ptr::null(),
                    if auto_reset { 0 } else { 1 },
                    if initially_signaled { 1 } else { 0 },
                    std::ptr::null(),
                )
            };
            if handle.is_null() {
                return Err(last_err("CreateEventW"));
            }
            Ok(Self {
                handle: owned(handle),
                auto_reset,
            })
        }
        /// 所有するハンドルを Event として使う。Event でないハンドルへの
        /// 操作は OS が失敗させる。
        pub fn from_owned(handle: OwnedHandle, auto_reset: bool) -> Self {
            Self { handle, auto_reset }
        }
        pub fn as_handle(&self) -> BorrowedHandle<'_> {
            self.handle.borrow()
        }
        pub fn is_auto_reset(&self) -> bool {
            self.auto_reset
        }
        /// 対象プロセスへ DuplicateHandle で渡す（Host→Agent の space_available
        /// 受け渡し。A68）。返した値は対象プロセス側のハンドル値。
        pub fn duplicate_to_process(&self, target_pid: u32) -> Result<RemoteHandle> {
            duplicate_to_process(self.handle.borrow(), target_pid)
        }
        pub fn make_inheritable(&self) -> Result<()> {
            make_inheritable(self.handle.borrow(), "SetHandleInformation(event)")
        }
        pub fn set(&self) -> Result<()> {
            // SAFETY: `self.handle` is an open event handle owned by this value.
            if unsafe { SetEvent(h(self.handle.borrow())) } == 0 {
                return Err(last_err("SetEvent"));
            }
            Ok(())
        }
        /// タイムアウト付き待機。true = signaled。
        ///
        /// 既定のタイマー周期 (約 15.6ms) より短い待機は、高分解能の
        /// waitable timer と一緒に待つ (2026-11-14 レビュー P2-14: 1ms の
        /// つもりの待機が既定周期まで伸び得た)。プロセス全体のタイマー
        /// 分解能 (timeBeginPeriod) は変えない: 対象アプリの時間の振る舞いを
        /// 記録のために変えないため。作れなければ従来の待機に戻る。
        pub fn wait_timeout(&self, timeout: Duration) -> bool {
            let ms = super::wait_timeout_millis(timeout);
            if ms > 0 && ms < super::PRECISE_WAIT_BELOW_MS {
                if let Some(signaled) = self.wait_precise(timeout) {
                    return signaled;
                }
            }
            // SAFETY: `self.handle` is an open event handle owned by this value.
            unsafe { WaitForSingleObject(h(self.handle.borrow()), ms) == 0 }
        }

        /// `timeout` まで高分解能タイマーと一緒に待つ。タイマーを作れ
        /// なければ None。
        fn wait_precise(&self, timeout: Duration) -> Option<bool> {
            use windows_sys::Win32::System::Threading::{
                CreateWaitableTimerExW, SetWaitableTimer, WaitForMultipleObjects,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
            };
            // SAFETY: an unnamed timer with default security, closed below.
            let timer = unsafe {
                CreateWaitableTimerExW(
                    std::ptr::null(),
                    std::ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                )
            };
            if timer.is_null() {
                return None;
            }
            // Relative due time in 100ns units (negative = relative).
            let due = -((timeout.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
            // SAFETY: valid timer handle, no completion routine.
            let armed = unsafe { SetWaitableTimer(timer, &due, 0, None, std::ptr::null(), 0) } != 0;
            let result = if armed {
                let handles = [h(self.handle.borrow()), timer];
                // The timer bounds the wait; INFINITE is never reached unless
                // the timer itself fails, guarded by a generous fallback.
                let fallback = super::wait_timeout_millis(timeout) + 16;
                // SAFETY: two valid handles.
                let r = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, fallback) };
                Some(r == 0)
            } else {
                None
            };
            // SAFETY: created above.
            unsafe { CloseHandle(timer) };
            result
        }
    }

    /// 継承ハンドル付きで子プロセスを起動する（E2E 用）。
    /// 呼び手が対象ハンドルへ make_inheritable 済みであること。
    pub fn spawn_inheriting(cmdline: &str) -> Result<OwnedHandle> {
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
        };
        let mut cmd: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `cmd` is a mutable NUL-terminated buffer and `si`/`pi` are initialised locals.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // inherit handles
                0,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(last_err("CreateProcessW"));
        }
        // SAFETY: `pi.hThread` was returned to us by CreateProcessW and is not used afterwards.
        unsafe {
            CloseHandle(pi.hThread);
        }
        Ok(owned(pi.hProcess))
    }
    /// 終了を `timeout` まで待ち、終了コードを返す。ハンドルは閉じない。
    pub fn wait_process(process: BorrowedHandle<'_>, timeout: Duration) -> Option<u32> {
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        let handle = h(process);
        let ms = super::wait_timeout_millis(timeout);
        // SAFETY: the borrowed process handle is open for the call.
        let r = unsafe { WaitForSingleObject(handle, ms) };
        if r != 0 {
            return None;
        }
        let mut code: u32 = 0;
        // SAFETY: the borrowed process handle is open and `code` is a writable local.
        unsafe {
            GetExitCodeProcess(handle, &mut code);
        }
        Some(code)
    }

    /// CREATE_SUSPENDED で起動する。メインスレッドは止まったままで、
    /// 呼び手が `resume_main_thread` するまで対象のコードは動かない
    /// （Agent の初期化完了を Host が確認してから再開する、overview の手順）。
    pub fn spawn_suspended(cmdline: &str) -> Result<SpawnedProcess> {
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
        };
        let mut cmd: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `cmd` is a mutable NUL-terminated buffer and `si`/`pi` are initialised locals.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_SUSPENDED,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(last_err("CreateProcessW(suspended)"));
        }
        Ok(SpawnedProcess {
            process: owned(pi.hProcess),
            thread: owned(pi.hThread),
            pid: pi.dwProcessId,
        })
    }

    /// 停止中のメインスレッドを再開する。戻り値は直前のサスペンド数。
    pub fn resume_main_thread(thread: BorrowedHandle<'_>) -> Result<u32> {
        use windows_sys::Win32::System::Threading::ResumeThread;
        // SAFETY: the borrowed thread handle is open for the call.
        let prev = unsafe { ResumeThread(h(thread)) };
        if prev == u32::MAX {
            return Err(last_err("ResumeThread"));
        }
        Ok(prev)
    }

    /// DLL 注入に必要な権限（スレッド作成・VM 操作・複製）でプロセスを開く。
    pub fn open_process_for_injection(pid: u32) -> Result<OwnedHandle> {
        use windows_sys::Win32::System::Threading::{
            PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE,
            PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
        };
        let access = PROCESS_CREATE_THREAD
            | PROCESS_QUERY_INFORMATION
            | PROCESS_VM_OPERATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE
            | PROCESS_DUP_HANDLE
            | PROCESS_SYNCHRONIZE;
        // SAFETY: OpenProcess takes only values; failure returns null, checked below.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            return Err(last_err("OpenProcess(inject)"));
        }
        Ok(owned(handle))
    }

    /// プロセスが終了済みかどうか（待たない確認）。
    pub fn process_exited(process: BorrowedHandle<'_>) -> Result<bool> {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        // SAFETY: the borrowed process handle is open for the call.
        match unsafe { WaitForSingleObject(h(process), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(last_err("WaitForSingleObject(process)")),
        }
    }

    /// Read an exited process's status without consuming its handle. Waiting
    /// first distinguishes a live process from one that actually exited with 259
    /// (STILL_ACTIVE).
    pub fn process_exit_code(process: BorrowedHandle<'_>) -> Result<Option<u32>> {
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        if !process_exited(process)? {
            return Ok(None);
        }
        let mut code = 0;
        // SAFETY: the borrowed process handle is open and `code` is a writable local.
        if unsafe { GetExitCodeProcess(h(process), &mut code) } == 0 {
            return Err(last_err("GetExitCodeProcess"));
        }
        Ok(Some(code))
    }

    /// Peak working set and peak private commit (bytes) of a process, from
    /// `GetProcessMemoryInfo`. The counters stay readable after the process
    /// exits while a handle is open.
    pub fn process_peak_memory(process: BorrowedHandle<'_>) -> Result<ProcessPeakMemory> {
        peak_memory_of(h(process))
    }
    /// [`process_peak_memory`] of this process.
    pub fn current_process_peak_memory() -> Result<ProcessPeakMemory> {
        // SAFETY: no preconditions; the pseudo handle names this process.
        peak_memory_of(unsafe { GetCurrentProcess() })
    }
    fn peak_memory_of(handle: HANDLE) -> Result<ProcessPeakMemory> {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: `handle` is the current process pseudo handle or a borrowed open one, and `counters` has the size passed.
        if unsafe { GetProcessMemoryInfo(handle, &mut counters, counters.cb) } == 0 {
            return Err(last_err("GetProcessMemoryInfo"));
        }
        Ok(ProcessPeakMemory {
            peak_working_set: counters.PeakWorkingSetSize as u64,
            peak_private: counters.PeakPagefileUsage as u64,
        })
    }

    pub fn process_creation_time(process: BorrowedHandle<'_>) -> Result<u64> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut times: [FILETIME; 4] = unsafe { std::mem::zeroed() };
        // SAFETY: the borrowed process handle is open and each FILETIME is a writable local.
        if unsafe {
            GetProcessTimes(
                h(process),
                &mut times[0],
                &mut times[1],
                &mut times[2],
                &mut times[3],
            )
        } == 0
        {
            return Err(last_err("GetProcessTimes"));
        }
        Ok((u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }

    /// 任意のハンドルを対象プロセスへ複製する（Host プロセスへの
    /// ハンドルを生存確認用に渡すなど、型付き API が無いものの受け渡し）。
    pub fn duplicate_handle_to_process(
        handle: BorrowedHandle<'_>,
        target_pid: u32,
    ) -> Result<RemoteHandle> {
        // SAFETY: OpenProcess takes only values; failure returns null, checked below.
        let target = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, target_pid) };
        if target.is_null() {
            return Err(last_err("OpenProcess"));
        }
        let mut dup: HANDLE = std::ptr::null_mut();
        // SAFETY: `handle` and `target` are open handles and `dup` is a writable local.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h(handle),
                target,
                &mut dup,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        // SAFETY: `target` was opened above, is not used after this, and is closed once.
        unsafe { CloseHandle(target) };
        if ok == 0 {
            return Err(last_err("DuplicateHandle"));
        }
        Ok(remote(dup, target_pid))
    }

    /// Give `target_pid` a handle to the current (Host) process that can only
    /// wait for its exit. The Agent uses it for liveness checks; it must not
    /// receive VM write, thread creation or handle duplication rights.
    pub fn duplicate_self_synchronize_to_process(target_pid: u32) -> Result<RemoteHandle> {
        use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
        // SAFETY: OpenProcess takes only values; failure returns null, checked below.
        let target = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, target_pid) };
        if target.is_null() {
            return Err(last_err("OpenProcess"));
        }
        let mut dup: HANDLE = std::ptr::null_mut();
        // SAFETY: the current process pseudo handle and `target` are valid and `dup` is a writable local.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                GetCurrentProcess(),
                target,
                &mut dup,
                PROCESS_SYNCHRONIZE,
                0,
                0,
            )
        };
        // SAFETY: `target` was opened above, is not used after this, and is closed once.
        unsafe { CloseHandle(target) };
        if ok == 0 {
            return Err(last_err("DuplicateHandle(host liveness)"));
        }
        Ok(remote(dup, target_pid))
    }

    /// Close a handle that this process duplicated into `target_pid`
    /// (`DUPLICATE_CLOSE_SOURCE`). Only for handles the target has not been
    /// told about: the target could otherwise already have closed and reused
    /// the value.
    pub fn close_remote_handle(value: RemoteHandle) -> Result<()> {
        use windows_sys::Win32::Foundation::DUPLICATE_CLOSE_SOURCE;
        if value.raw == 0 {
            return Ok(());
        }
        let target_pid = value.target_pid;
        // SAFETY: OpenProcess takes only values; failure returns null, checked below.
        let target = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, target_pid) };
        if target.is_null() {
            return Err(last_err("OpenProcess"));
        }
        // SAFETY: `target` is open; DUPLICATE_CLOSE_SOURCE closes `value` in that process and returns nothing here.
        let ok = unsafe {
            DuplicateHandle(
                target,
                value.raw as HANDLE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                0,
                DUPLICATE_CLOSE_SOURCE,
            )
        };
        // SAFETY: `target` was opened above, is not used after this, and is closed once.
        unsafe { CloseHandle(target) };
        if ok == 0 {
            return Err(last_err("DuplicateHandle(close remote)"));
        }
        Ok(())
    }

    /// Copies `src` to address `dest` of the target process. The target
    /// address is checked by the kernel (an invalid range is an error).
    pub fn write_process_memory(
        process: BorrowedHandle<'_>,
        dest: usize,
        src: &[u8],
    ) -> Result<()> {
        use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
        let mut written: usize = 0;
        // SAFETY: `src` is a live slice of `src.len()` bytes and `written` is a
        // writable local; the remote range is validated by the kernel.
        let ok = unsafe {
            WriteProcessMemory(
                h(process),
                dest as *const c_void,
                src.as_ptr() as *const c_void,
                src.len(),
                &mut written,
            )
        };
        if ok == 0 || written != src.len() {
            return Err(last_err("WriteProcessMemory"));
        }
        Ok(())
    }

    /// 対象プロセスのメモリを読む (Bootstrap の初期化結果の読み戻し等)。
    pub fn read_process_memory(
        process: BorrowedHandle<'_>,
        src: usize,
        dest: &mut [u8],
    ) -> Result<()> {
        use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        let mut read: usize = 0;
        // SAFETY: `dest` is a live, exclusively borrowed slice of `dest.len()`
        // bytes; the remote range is validated by the kernel.
        let ok = unsafe {
            ReadProcessMemory(
                h(process),
                src as *const c_void,
                dest.as_mut_ptr() as *mut c_void,
                dest.len(),
                &mut read,
            )
        };
        if ok == 0 || read != dest.len() {
            return Err(last_err("ReadProcessMemory"));
        }
        Ok(())
    }

    /// 対象プロセスで `start(param)` をリモートスレッドで実行し、戻り値
    /// （スレッド終了コード）を返す。30 秒で打ち切る。
    pub fn remote_call(process: BorrowedHandle<'_>, start: usize, param: usize) -> Result<u32> {
        remote_call_with_timeout(process, start, param, Duration::from_secs(30))
    }

    fn remote_call_with_timeout(
        process: BorrowedHandle<'_>,
        start: usize,
        param: usize,
        timeout: Duration,
    ) -> Result<u32> {
        use windows_sys::Win32::System::Threading::{
            CreateRemoteThread, GetExitCodeThread, LPTHREAD_START_ROUTINE,
        };
        // SAFETY: start は対象プロセス内で有効な関数アドレスであること
        // （呼び出し側が LoadLibrary 済みモジュールのベース + RVA で保証する）。
        let routine: LPTHREAD_START_ROUTINE =
            unsafe { std::mem::transmute(start as *const c_void) };
        // SAFETY: `routine` is a function in the target process (caller's contract above) and the process handle is open.
        let thread = unsafe {
            CreateRemoteThread(
                h(process),
                std::ptr::null(),
                0,
                routine,
                param as *mut c_void,
                0,
                std::ptr::null_mut(),
            )
        };
        if thread.is_null() {
            return Err(last_err("CreateRemoteThread"));
        }
        // SAFETY: `thread` was created above and is still open.
        let waited = unsafe { WaitForSingleObject(thread, super::wait_timeout_millis(timeout)) };
        if waited != 0 {
            // SAFETY: `thread` was created above and is closed once on this path.
            unsafe { CloseHandle(thread) };
            return Err(WinError::RemoteCallPending {
                wait_status: waited,
            });
        }
        let mut code: u32 = 0;
        // SAFETY: `thread` is open and `code` is a writable local.
        let ok = unsafe { GetExitCodeThread(thread, &mut code) };
        // SAFETY: `thread` was created above and is closed once on this path.
        unsafe { CloseHandle(thread) };
        if ok == 0 {
            return Err(last_err("GetExitCodeThread"));
        }
        Ok(code)
    }

    /// 対象プロセスで `LoadLibraryW(dll_path)` を実行する。
    /// 前提： 対象と同一ビットネス（自プロセスの kernel32 の
    /// LoadLibraryW アドレスが対象でも有効。同一ブート・同一
    /// ビットネスの system DLL はプロセス間で同じベースを持つ）。
    pub fn remote_load_library(process: BorrowedHandle<'_>, dll_path: &str) -> Result<()> {
        remote_load_library_with_call(process, dll_path, remote_call)
    }

    fn remote_load_library_with_call(
        process: BorrowedHandle<'_>,
        dll_path: &str,
        call: impl FnOnce(BorrowedHandle<'_>, usize, usize) -> Result<u32>,
    ) -> Result<()> {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        use windows_sys::Win32::System::Memory::{
            VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
        };
        let proc_handle = h(process);
        let wide: Vec<u16> = dll_path.encode_utf16().chain([0]).collect();
        let bytes = wide.len() * 2;
        // SAFETY: the process handle is open; failure returns null, checked below.
        let remote = unsafe {
            VirtualAllocEx(
                proc_handle,
                std::ptr::null(),
                bytes,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if remote.is_null() {
            return Err(last_err("VirtualAllocEx"));
        }
        let result = (|| -> Result<()> {
            let wide_bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
            write_process_memory(process, remote as usize, &wide_bytes)?;
            let k32_name: Vec<u16> = "kernel32.dll".encode_utf16().chain([0]).collect();
            // SAFETY: the name is a NUL-terminated UTF-16 string that outlives the call.
            let k32 = unsafe { GetModuleHandleW(k32_name.as_ptr()) };
            if k32.is_null() {
                return Err(last_err("GetModuleHandleW"));
            }
            // SAFETY: `k32` is the loaded kernel32 module and the name is a NUL-terminated C string.
            let load = unsafe { GetProcAddress(k32, c"LoadLibraryW".as_ptr() as *const u8) };
            let Some(load) = load else {
                return Err(WinError::OsError {
                    api: "GetProcAddress(LoadLibraryW)",
                    code: 0,
                });
            };
            let code = call(process, load as usize, remote as usize)?;
            if code == 0 {
                return Err(WinError::OsError {
                    api: "LoadLibraryW(remote) returned NULL",
                    code: 0,
                });
            }
            Ok(())
        })();
        // A timeout does not stop LoadLibraryW. Its input must remain valid
        // until the remote thread finishes (or the target process exits).
        if !matches!(result, Err(WinError::RemoteCallPending { .. })) {
            // SAFETY: `remote` was allocated above in that process and the remote thread is no longer using it.
            unsafe { VirtualFreeEx(proc_handle, remote, 0, MEM_RELEASE) };
        }
        result
    }

    /// 実行中の全プロセスの (pid, 親 pid) 一覧を返す
    /// （L05 の子プロセス発見用。Toolhelp のプロセススナップショット）。
    pub fn enumerate_processes() -> Result<Vec<(u32, u32)>> {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        // SAFETY: a process snapshot takes only values; failure returns INVALID_HANDLE_VALUE, checked below.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return Err(last_err("CreateToolhelp32Snapshot"));
        }
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut out = Vec::new();
        // SAFETY: `snap` is the open snapshot and `entry` is initialised with its size; the snapshot is closed once.
        unsafe {
            if Process32FirstW(snap, &mut entry) != 0 {
                loop {
                    out.push((entry.th32ProcessID, entry.th32ParentProcessID));
                    if Process32NextW(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        Ok(out)
    }

    /// 対象プロセスのモジュール一覧から、絶対パスが一致するモジュールの
    /// ベースを引く。ファイル名だけで照合すると、対象の検索順で読み込まれた
    /// 同名の別 DLL を Host が解析した DLL と取り違える。
    pub fn module_base(pid: u32, module_path: &str) -> Result<usize> {
        if !super::is_absolute_windows_path(module_path) {
            return Err(WinError::InvalidArgument("module path must be absolute"));
        }
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W,
            TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
        };
        let snap =
            // SAFETY: a module snapshot takes only values; failure returns INVALID_HANDLE_VALUE, checked below.
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
        if snap == INVALID_HANDLE_VALUE {
            return Err(last_err("CreateToolhelp32Snapshot"));
        }
        // SAFETY: a plain C struct; all-zero bytes are a valid value.
        let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut result = Err(WinError::InvalidArgument("module not loaded in target"));
        // SAFETY: `snap` is the open snapshot and `entry` is initialised with its size; the snapshot is closed once.
        unsafe {
            if Module32FirstW(snap, &mut entry) != 0 {
                loop {
                    let path_len = entry
                        .szExePath
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExePath.len());
                    let path = String::from_utf16_lossy(&entry.szExePath[..path_len]);
                    if super::same_windows_path(&path, module_path) {
                        result = Ok(entry.modBaseAddr as usize);
                        break;
                    }
                    if Module32NextW(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        result
    }

    /// 対象プロセス内にメモリを確保する（Bootstrap の受け渡し用）。
    pub fn remote_alloc(process: BorrowedHandle<'_>, len: usize) -> Result<usize> {
        use windows_sys::Win32::System::Memory::{
            VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
        };
        // SAFETY: the process handle is open; failure returns null, checked below.
        let addr = unsafe {
            VirtualAllocEx(
                h(process),
                std::ptr::null(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if addr.is_null() {
            return Err(last_err("VirtualAllocEx(bootstrap)"));
        }
        Ok(addr as usize)
    }

    /// `remote_alloc` で確保したメモリを解放する。
    ///
    /// # Safety
    /// `addr` is 0 or an address `remote_alloc` returned for `process`, and
    /// nothing (in that process, or here if it is the current one) uses it
    /// any more.
    pub unsafe fn remote_free(process: BorrowedHandle<'_>, addr: usize) {
        use windows_sys::Win32::System::Memory::{VirtualFreeEx, MEM_RELEASE};
        if addr != 0 {
            // SAFETY: the caller passes an address that remote_alloc returned for this process.
            unsafe { VirtualFreeEx(h(process), addr as *mut c_void, 0, MEM_RELEASE) };
        }
    }

    /// プロセスを終了させる。起動に失敗した対象の後始末専用
    /// （他人のプロセスに使わないこと）。
    pub fn terminate_process(process: BorrowedHandle<'_>, exit_code: u32) -> Result<()> {
        use windows_sys::Win32::System::Threading::TerminateProcess;
        // SAFETY: the borrowed process handle is open for the call.
        let ok = unsafe { TerminateProcess(h(process), exit_code) };
        if ok == 0 {
            return Err(last_err("TerminateProcess"));
        }
        Ok(())
    }
    #[cfg(test)]
    mod remote_parameter_tests {
        use super::*;
        use std::sync::atomic::{AtomicU32, AtomicUsize};

        #[test]
        fn timed_out_load_keeps_parameter_for_delayed_native_reader() {
            static GATE: AtomicUsize = AtomicUsize::new(0);
            static DONE: AtomicUsize = AtomicUsize::new(0);
            static OBSERVED: AtomicU32 = AtomicU32::new(u32::MAX);

            unsafe extern "system" fn delayed_read(parameter: *mut c_void) -> u32 {
                let waited =
                    // SAFETY: GATE holds the event created before this thread started, open until the test closes it.
                    unsafe { WaitForSingleObject(GATE.load(Ordering::SeqCst) as HANDLE, 5_000) };
                if waited == 0 {
                    let mut first = [0u8; 2];
                    // ReadProcessMemory reports a freed allocation without
                    // deliberately crashing the test process on an invalid read.
                    if read_process_memory(current_process(), parameter as usize, &mut first)
                        .is_ok()
                    {
                        OBSERVED.store(u16::from_le_bytes(first) as u32, Ordering::SeqCst);
                    }
                }
                // SAFETY: DONE holds the event created before this thread started, open until the test closes it.
                unsafe { SetEvent(DONE.load(Ordering::SeqCst) as HANDLE) };
                1
            }

            let process = current_process();
            // SAFETY: an anonymous event with default security; failure returns null, checked below.
            let gate = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            // SAFETY: an anonymous event with default security; failure returns null, checked below.
            let done = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            assert!(!gate.is_null() && !done.is_null());
            GATE.store(gate as usize, Ordering::SeqCst);
            DONE.store(done as usize, Ordering::SeqCst);
            let mut parameter = 0;
            let result = remote_load_library_with_call(process, "Q-test.dll", |process, _, ptr| {
                parameter = ptr;
                remote_call_with_timeout(
                    process,
                    delayed_read as *const () as usize,
                    ptr,
                    Duration::ZERO,
                )
            });
            // The thread cannot complete before this signal, irrespective of
            // scheduler speed. Its delayed read happens after the timeout path.
            // SAFETY: `gate` was created above and is still open.
            unsafe { SetEvent(gate) };
            // SAFETY: `done` was created above and is still open.
            let finished = unsafe { WaitForSingleObject(done, 5_000) } == 0;
            let observed = OBSERVED.load(Ordering::SeqCst);
            if finished {
                // The callback has finished using parameter before signaling.
                // SAFETY: `parameter` came from remote_alloc above and the
                // callback signalled DONE after its last use.
                unsafe { remote_free(process, parameter) };
                // SAFETY: both events were created above and are closed once, after the thread finished with them.
                unsafe {
                    CloseHandle(gate);
                    CloseHandle(done);
                }
            } // On failure retain resources rather than close a live wait.
            assert!(matches!(
                result,
                Err(WinError::RemoteCallPending { wait_status: 0x102 })
            ));
            assert!(finished, "native reader did not finish");
            assert_eq!(
                observed,
                u32::from(b'Q'),
                "parameter was freed before the late read"
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub struct SharedMapping {
        shm: Shm,
    }
    impl SharedMapping {
        pub fn create(len: usize) -> Result<Self> {
            // スタブ: ローカルヒープで代替し API 形だけ保つ（プロセス間共有ではない）
            let shm = Shm::alloc_zeroed(len).map_err(|_| WinError::InvalidArgument("len"))?;
            Ok(Self { shm })
        }
        /// スタブ: アクセス制御は Windows の概念のため Unsupported。
        pub fn create_restricted(_len: usize) -> Result<Self> {
            Err(WinError::Unsupported)
        }
        /// スタブ: ハンドルからの写像は Windows の概念のため Unsupported。
        pub fn from_owned(_handle: OwnedHandle, _len: usize) -> Result<Self> {
            Err(WinError::Unsupported)
        }
        pub fn duplicate_to_process(&self, _target_pid: u32) -> Result<RemoteHandle> {
            Err(WinError::Unsupported)
        }
        pub fn make_inheritable(&self) -> Result<()> {
            Err(WinError::Unsupported)
        }
        pub fn shm(&self) -> &Shm {
            &self.shm
        }
        pub fn len(&self) -> usize {
            self.shm.len()
        }
        pub fn is_empty(&self) -> bool {
            self.shm.is_empty()
        }
    }
    pub struct EventObject {
        auto_reset: bool,
        // Windows の Event と同じ意味論のスタブ。condvar で真のブロッキング待機をする
        // （1ms ポーリングでは待機の遅延が Windows 実機と大きく変わってしまうため）。
        // auto-reset: 待機がシグナルを消費する。現状の全用途は auto-reset。
        state: std::sync::Mutex<bool>,
        condvar: std::sync::Condvar,
    }
    impl EventObject {
        pub fn new(auto_reset: bool, initially_signaled: bool) -> Result<Self> {
            Ok(Self {
                auto_reset,
                state: std::sync::Mutex::new(initially_signaled),
                condvar: std::sync::Condvar::new(),
            })
        }
        /// スタブではハンドルを使わない。
        pub fn from_owned(_handle: OwnedHandle, auto_reset: bool) -> Self {
            Self {
                auto_reset,
                state: std::sync::Mutex::new(false),
                condvar: std::sync::Condvar::new(),
            }
        }
        pub fn is_auto_reset(&self) -> bool {
            self.auto_reset
        }
        /// スタブ: プロセス間ハンドル受け渡しは Windows の概念のため Unsupported。
        pub fn duplicate_to_process(&self, _target_pid: u32) -> Result<RemoteHandle> {
            Err(WinError::Unsupported)
        }
        pub fn make_inheritable(&self) -> Result<()> {
            Err(WinError::Unsupported)
        }
        pub fn set(&self) -> Result<()> {
            let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
            *g = true;
            self.condvar.notify_one();
            Ok(())
        }
        pub fn wait_timeout(&self, timeout: Duration) -> bool {
            let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if *g {
                *g = false;
                return true;
            }
            let (mut g2, _t) = self
                .condvar
                .wait_timeout(g, timeout)
                .unwrap_or_else(|e| e.into_inner());
            if *g2 {
                *g2 = false;
                true
            } else {
                false
            }
        }
    }
    pub fn spawn_inheriting(_cmdline: &str) -> Result<OwnedHandle> {
        Err(WinError::Unsupported)
    }
    pub fn wait_process(_h: BorrowedHandle<'_>, _t: Duration) -> Option<u32> {
        None
    }
    pub fn spawn_suspended(_cmdline: &str) -> Result<SpawnedProcess> {
        Err(WinError::Unsupported)
    }
    pub fn resume_main_thread(_thread: BorrowedHandle<'_>) -> Result<u32> {
        Err(WinError::Unsupported)
    }
    pub fn open_process_for_injection(_pid: u32) -> Result<OwnedHandle> {
        Err(WinError::Unsupported)
    }
    pub fn process_exited(_process: BorrowedHandle<'_>) -> Result<bool> {
        Err(WinError::Unsupported)
    }
    pub fn duplicate_handle_to_process(
        _handle: BorrowedHandle<'_>,
        _target_pid: u32,
    ) -> Result<RemoteHandle> {
        Err(WinError::Unsupported)
    }
    pub fn duplicate_self_synchronize_to_process(_target_pid: u32) -> Result<RemoteHandle> {
        Err(WinError::Unsupported)
    }
    pub fn close_remote_handle(value: RemoteHandle) -> Result<()> {
        let _ = value;
        Err(WinError::Unsupported)
    }
    pub fn write_process_memory(
        _process: BorrowedHandle<'_>,
        _dest: usize,
        _src: &[u8],
    ) -> Result<()> {
        Err(WinError::Unsupported)
    }
    pub fn read_process_memory(
        _process: BorrowedHandle<'_>,
        _src: usize,
        _dest: &mut [u8],
    ) -> Result<()> {
        Err(WinError::Unsupported)
    }
    pub fn remote_call(_process: BorrowedHandle<'_>, _start: usize, _param: usize) -> Result<u32> {
        Err(WinError::Unsupported)
    }
    pub fn remote_load_library(_process: BorrowedHandle<'_>, _dll_path: &str) -> Result<()> {
        Err(WinError::Unsupported)
    }
    pub fn module_base(_pid: u32, _module_path: &str) -> Result<usize> {
        Err(WinError::Unsupported)
    }
    /// スタブ: プロセス一覧は Windows の概念のため Unsupported。
    pub fn enumerate_processes() -> Result<Vec<(u32, u32)>> {
        Err(WinError::Unsupported)
    }
    pub fn remote_alloc(_process: BorrowedHandle<'_>, _len: usize) -> Result<usize> {
        Err(WinError::Unsupported)
    }
    /// # Safety
    /// Same contract as the Windows version (this stub does nothing).
    pub unsafe fn remote_free(_process: BorrowedHandle<'_>, _addr: usize) {}
    pub fn terminate_process(_process: BorrowedHandle<'_>, _exit_code: u32) -> Result<()> {
        Err(WinError::Unsupported)
    }
    pub fn process_exit_code(_process: BorrowedHandle<'_>) -> Result<Option<u32>> {
        Err(WinError::Unsupported)
    }
    pub fn process_creation_time(_process: BorrowedHandle<'_>) -> Result<u64> {
        Err(WinError::Unsupported)
    }
    pub fn process_peak_memory(_process: BorrowedHandle<'_>) -> Result<ProcessPeakMemory> {
        Err(WinError::Unsupported)
    }
    pub fn current_process_peak_memory() -> Result<ProcessPeakMemory> {
        Err(WinError::Unsupported)
    }
}

pub use imp::{
    close_remote_handle, current_process_peak_memory, duplicate_handle_to_process,
    duplicate_self_synchronize_to_process, enumerate_processes, module_base,
    open_process_for_injection, process_creation_time, process_exit_code, process_exited,
    process_peak_memory, read_process_memory, remote_alloc, remote_call, remote_free,
    remote_load_library, resume_main_thread, spawn_inheriting, spawn_suspended, terminate_process,
    wait_process, write_process_memory, EventObject, SharedMapping,
};

// --- 利用者による記録停止 (2026-11-14 レビュー P1-5) ---

static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static STOP_FINISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 記録の停止を求める (Ctrl-C / Ctrl-Break / コンソールを閉じる操作)。
pub fn request_stop() {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::Release);
}

/// 停止が求められたか。回収ループは対象の終了と同じく、これを見て
/// 抜け、Index と Finalization を書いて閉じる。
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(std::sync::atomic::Ordering::Acquire)
}

/// 停止後の確定が終わったことを知らせる (コンソールを閉じる操作では、
/// ハンドラが戻るとプロセスが終了するため、それまで待つ)。
pub fn notify_stop_finished() {
    STOP_FINISHED.store(true, std::sync::atomic::Ordering::Release);
}

/// コンソールの Ctrl-C / Ctrl-Break / 閉じる操作を「記録の停止」として
/// 受けるハンドラを登録する。1 回目は停止を求めるだけでプロセスを
/// 終わらせない。2 回目は既定の処理 (強制終了) に任せる。
#[cfg(windows)]
pub fn install_console_stop_handler() -> bool {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
    };
    unsafe extern "system" fn handler(ctrl: u32) -> BOOL {
        match ctrl {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => {
                if stop_requested() {
                    return 0; // 2 回目: 既定の処理で終わらせる
                }
                request_stop();
                if ctrl == CTRL_CLOSE_EVENT {
                    // 戻ると終了させられる。確定を最大 4.5 秒待つ。
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(4500);
                    while !STOP_FINISHED.load(std::sync::atomic::Ordering::Acquire)
                        && std::time::Instant::now() < deadline
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
                1
            }
            _ => 0,
        }
    }
    // SAFETY: handler は 'static な関数で、登録の解除はしない。
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) != 0 }
}

/// Windows 以外では登録しない。
#[cfg(not(windows))]
pub fn install_console_stop_handler() -> bool {
    false
}

/// `C:\...` or `\\server\share\...` (either separator). Relative and
/// drive-relative paths are resolved by the target's own search rules.
pub fn is_absolute_windows_path(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
        || (b.len() >= 3 && matches!(b[0], b'\\' | b'/') && matches!(b[1], b'\\' | b'/'))
}

/// Compare loader paths the way NTFS names compare in practice for this use:
/// separators are equivalent and ASCII case is ignored.
pub fn same_windows_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .map(|c| {
                if c == '/' {
                    '\\'
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect::<String>()
    };
    norm(a) == norm(b)
}

/// プロセス単位の通知イベント群。data_ready は auto-reset。
pub struct ProcessNotify {
    pub data_ready: EventObject,
}
impl ProcessNotify {
    pub fn new() -> Result<Self> {
        Ok(Self {
            data_ready: EventObject::new(true, false)?,
        })
    }
    pub fn notify_data_ready(&self) -> Result<()> {
        self.data_ready.set()
    }
    /// 2ms 再確認付きで待つ。通知を取りこぼしても再確認で必ず戻る。
    ///
    /// 1 回の待機スライスは「その反復で待つ長さ」であり、毎回
    /// `policy.interval` を上限に、残り時間で切り取る (R4)。累積経過を
    /// `ReCheckPolicy::wait_timeout` (待機開始からの経過に対する残り) に
    /// 渡すと、interval 経過後にスライスが 0 になり、ゼロ待機の
    /// ビジーループになるため、ここでは使わない。
    pub fn wait_with_recheck(
        &self,
        policy: ReCheckPolicy,
        mut recheck: impl FnMut() -> bool,
        total_timeout: Duration,
    ) -> bool {
        let start = std::time::Instant::now();
        loop {
            if recheck() {
                return true;
            }
            let waited = start.elapsed();
            if waited >= total_timeout {
                return recheck();
            }
            let slice = policy.interval.min(total_timeout - waited);
            if self.data_ready.wait_timeout(slice) && recheck() {
                return true;
            }
        }
    }
}

/// Ring 単位の空き通知。
pub struct RingNotify {
    pub space_available: EventObject,
}
impl RingNotify {
    pub fn new() -> Result<Self> {
        Ok(Self {
            space_available: EventObject::new(true, false)?,
        })
    }
    pub fn notify_space(&self) -> Result<()> {
        self.space_available.set()
    }
    /// Host→Agent の受け渡し: `space_available` イベントを対象プロセスへ
    /// DuplicateHandle で渡す（A68）。返した値は対象プロセス側のハンドル値で、
    /// Agent は `EventObject::from_owned` で取り込む。Windows 以外は
    /// Unsupported（スタブにプロセス間の実体が無いため）。
    pub fn duplicate_space_to_process(&self, target_pid: u32) -> Result<RemoteHandle> {
        self.space_available.duplicate_to_process(target_pid)
    }
}

/// Host 側: 回収で空きができ、書き手が空き待ち中なら `space_available` を SetEvent する。
/// 書き手は待機の意思表示＋全順序フェンスの後に空きを再確認しているので、この通知と
/// 再確認の組合せで空き待ちの取りこぼしが起きない。
pub fn notify_space_if_writer_waiting(
    control: shirusu_transport::Control<'_>,
    notify: &RingNotify,
) -> Result<bool> {
    if control.writer_waiting() {
        notify.notify_space()?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// 書き手の待機を `space_available` イベントで実現する `Clock`。
/// `bounded_wait` は `Clock::sleep` を 50µs スライスで呼ぶ。実 sleep だと空きが
/// できても次のスライス境界まで起きないが、この Clock では回収時の SetEvent で
/// 即座に起きる（ベンチマーク A65 の選定結果: Event）。
/// Windows の `WaitForSingleObject` は ms 単位のため、タイムアウトは 1ms へ
/// 切り上げる。待機上限の判定は `now_ticks`（実時間）で行うため、上限を大きく
/// 超えて待つことはない。`space_available` は auto-reset で、待機 1 回につき
/// シグナルを 1 回消費する（Ring 単位で書き手は 1 人の前提）。
pub struct EventClock {
    notify: Arc<RingNotify>,
    origin: Instant,
    calls: AtomicU64,
}
impl EventClock {
    pub fn new(notify: Arc<RingNotify>) -> Self {
        Self {
            notify,
            origin: Instant::now(),
            calls: AtomicU64::new(0),
        }
    }
    pub fn sleep_calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}
impl shirusu_transport::Clock for EventClock {
    fn now_ticks(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }
    fn ticks_per_second(&self) -> u64 {
        1_000_000_000
    }
    fn sleep(&self, dur: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // イベントが立ていれば即復帰する。立っていなければタイムアウトまで待つ。
        let wait = dur.max(Duration::from_millis(1));
        let _ = self.notify.space_available.wait_timeout(wait);
    }
}

/// 本物の QPC を返すイベント時刻源 (レビュー第2回・項目8)。
///
/// Descriptor の `timestamp_qpc` はプロセス間で比較できることが仕様の
/// 要求であり、Windows では `QueryPerformanceCounter` の値そのものが
/// それに当たる。周波数は `QueryPerformanceFrequency` で取得して保持
/// する (起動後に変わらない値)。待機実測用の `Clock` とは別系統で、
/// Agent は Writer 構築時にこちらを `TimestampSource` として渡す。
/// Windows 以外では transport の既定時刻源へ委譲する (API 形のスタブ)。
pub struct QpcTimestampSource {
    frequency: u64,
    /// Ring の書き手が TSC で補間してよいか。Windows かつ invariant TSC の
    /// CPU だけで有効にする (値は常に QPC 単位。transport の `tsc` を参照)。
    /// `SHIRUSU_EVENT_CLOCK=qpc` で無効化できる (比較測定・問題切り分け用)。
    tsc_interpolation: bool,
    #[cfg(not(windows))]
    fallback: shirusu_transport::SystemTimestampSource,
}
impl QpcTimestampSource {
    pub fn new() -> Self {
        Self {
            frequency: qpc_frequency(),
            tsc_interpolation: cfg!(windows)
                && shirusu_transport::tsc::invariant_tsc_available()
                && std::env::var_os("SHIRUSU_EVENT_CLOCK").is_none_or(|v| v != "qpc"),
            #[cfg(not(windows))]
            fallback: shirusu_transport::SystemTimestampSource::new(),
        }
    }
    /// 書き手が TSC 補間を使う設定かどうか (診断・測定の記録用)。
    pub fn uses_tsc_interpolation(&self) -> bool {
        self.tsc_interpolation
    }
}
impl Default for QpcTimestampSource {
    fn default() -> Self {
        Self::new()
    }
}
/// Scheduling priority hint for Host threads (no-op off Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPriority {
    /// Short, latency-critical work that mostly sleeps (Ring collection).
    Highest,
    /// Throughput work that must not compete with the target or collection.
    BelowNormal,
    /// Default priority. Throughput work is raised to it while collection
    /// waits on it, so a CPU-bound target cannot starve it (review N3).
    Normal,
}

/// Set the calling thread's priority. Returns whether it was applied.
/// With fewer cores than busy threads, Ring collection must preempt Trace
/// conversion promptly; otherwise the writer overflows while it waits.
pub fn set_current_thread_priority(priority: ThreadPriority) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::GetCurrentThread;
        // SAFETY: the pseudo handle names the calling thread.
        unsafe { set_priority_of_handle(GetCurrentThread(), priority) }
    }
    #[cfg(not(windows))]
    {
        let _ = priority;
        false
    }
}

/// Set another Host thread's priority (for example, raise a starved worker
/// from the thread waiting on it). Returns whether it was applied.
pub fn set_thread_priority<T>(
    thread: &std::thread::JoinHandle<T>,
    priority: ThreadPriority,
) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the JoinHandle owns a valid thread handle while borrowed.
        unsafe { set_priority_of_handle(thread.as_raw_handle(), priority) }
    }
    #[cfg(not(windows))]
    {
        let _ = (thread, priority);
        false
    }
}

#[cfg(windows)]
unsafe fn set_priority_of_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
    priority: ThreadPriority,
) -> bool {
    use windows_sys::Win32::System::Threading::{
        SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_HIGHEST,
        THREAD_PRIORITY_NORMAL,
    };
    let value = match priority {
        ThreadPriority::Highest => THREAD_PRIORITY_HIGHEST,
        ThreadPriority::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL,
        ThreadPriority::Normal => THREAD_PRIORITY_NORMAL,
    };
    // SAFETY: caller passes a valid thread handle (or the pseudo handle).
    unsafe { SetThreadPriority(handle, value) != 0 }
}

/// Owner side of an asymmetric Dekker handshake: orders the owner's flag
/// store before its following load of the remote flag. On Windows the remote
/// side's [`asymmetric_fence_heavy`] serializes every processor, so the
/// owner only needs to stop compiler reordering and pays no fence or atomic
/// read-modify-write per operation. Elsewhere it is a full fence.
#[inline(always)]
pub fn asymmetric_fence_light() {
    #[cfg(windows)]
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    #[cfg(not(windows))]
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// Remote side of the asymmetric handshake (rare, management path). After
/// it returns, every owner's store issued before its light fence is visible
/// to the caller, or the owner will observe the caller's earlier store.
pub fn asymmetric_fence_heavy() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::FlushProcessWriteBuffers;
        // SAFETY: no arguments; flushes write buffers of every processor
        // running a thread of this process.
        unsafe { FlushProcessWriteBuffers() };
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// Proof that an OS thread has terminated (review P0, forced writer
/// termination). Taken by the thread itself on a cold path, when it first
/// claims a Ring writer or a hazard slot.
///
/// A thread ended by `TerminateThread` (or by `ExitThread` from a nested
/// frame) runs no Rust `Drop`, so a flag it raised for the duration of a
/// record stays raised. Waiting for the flag then never ends, and clearing it
/// after a timeout is unsound: a thread that was only suspended or descheduled
/// would resume inside its critical section. Termination is the only state in
/// which the flag can be ignored, and the kernel reports it exactly:
///
/// - Windows: a duplicated `SYNCHRONIZE` handle to the thread. The thread
///   object is signaled once the thread has terminated, and a terminated
///   thread never runs again. Holding the handle also keeps the thread ID
///   from being reused while the liveness is held, so the thread cannot be
///   confused with a later thread of the same ID.
/// - Elsewhere: a thread-local token cleared by its destructor. There is no
///   `TerminateThread` there; tests model forced termination as a thread that
///   leaks its guard (`mem::forget`) and exits.
pub struct ThreadLiveness {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(not(windows))]
    alive: Arc<std::sync::atomic::AtomicBool>,
}

// SAFETY: the handle is only waited on and closed; it is not tied to the
// thread that opened it.
unsafe impl Send for ThreadLiveness {}
// SAFETY: shared use only waits on the handle, which the kernel allows from any thread.
unsafe impl Sync for ThreadLiveness {}

#[cfg(not(windows))]
struct AliveToken(Arc<std::sync::atomic::AtomicBool>);
#[cfg(not(windows))]
impl Drop for AliveToken {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
#[cfg(not(windows))]
thread_local! {
    static ALIVE_TOKEN: AliveToken =
        AliveToken(Arc::new(std::sync::atomic::AtomicBool::new(true)));
}

impl ThreadLiveness {
    /// Liveness of the calling thread. `None` when it cannot be taken (for
    /// example during thread teardown); the caller must then treat the thread
    /// as alive forever.
    pub fn current() -> Option<Self> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{DuplicateHandle, HANDLE};
            use windows_sys::Win32::System::Threading::{
                GetCurrentProcess, GetCurrentThread, THREAD_SYNCHRONIZE,
            };
            let mut handle: HANDLE = std::ptr::null_mut();
            // SAFETY: duplicates the calling thread's pseudo handle into a
            // real handle of this process with SYNCHRONIZE access only.
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    GetCurrentThread(),
                    GetCurrentProcess(),
                    &mut handle,
                    THREAD_SYNCHRONIZE,
                    0,
                    0,
                )
            };
            (ok != 0 && !handle.is_null()).then_some(Self { handle })
        }
        #[cfg(not(windows))]
        {
            ALIVE_TOKEN
                .try_with(|t| Self {
                    alive: Arc::clone(&t.0),
                })
                .ok()
        }
    }

    /// Whether the thread has terminated. `true` is final: the thread will
    /// never execute again, and all its memory writes are complete.
    pub fn has_terminated(&self) -> bool {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            // SAFETY: the handle is owned by self and valid until drop.
            unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
        }
        #[cfg(not(windows))]
        {
            !self.alive.load(Ordering::Acquire)
        }
    }
}

/// Identity of an OS thread that another thread can later ask about
/// ([`ThreadKey::has_terminated`]) without holding a handle to it. Never 0,
/// so 0 can mean "no holder" where a key is stored in an atomic.
///
/// Windows: the thread ID (low 32 bits) and the low 32 bits of its creation
/// time (high 32 bits). Thread IDs are reused as soon as a thread object is
/// freed; the creation time tells a later thread with the same ID apart from
/// the one the key was taken from (review P1-2). Taken once per thread and
/// cached in TLS, so later calls make no system call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ThreadKey(std::num::NonZeroU64);

/// What a lookup of a [`ThreadKey`] found (pure decision, tested on every
/// platform).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyLookup {
    /// No thread has the ID any more (`OpenThread` failed with
    /// `ERROR_INVALID_PARAMETER`).
    NoSuchThread,
    /// The thread exists but could not be opened or queried (access denied,
    /// out of handles, ...). Its state is unknown.
    Unknown,
    /// The thread with this ID was opened.
    Opened {
        /// Its object is signaled: it terminated.
        signaled: bool,
        /// Low 32 bits of its creation time, if it could be read.
        birth: Option<u32>,
    },
}

impl KeyLookup {
    /// Whether the thread the key was taken from has terminated. Only proof
    /// counts: an unknown state reads as alive (the caller keeps waiting, as
    /// it would for a live holder), never as terminated.
    pub fn terminated(self, key_birth: u32) -> bool {
        match self {
            KeyLookup::NoSuchThread => true,
            KeyLookup::Unknown => false,
            KeyLookup::Opened { signaled: true, .. } => true,
            // The ID now belongs to a later thread: the keyed one is gone.
            KeyLookup::Opened {
                signaled: false,
                birth: Some(birth),
            } => birth != key_birth,
            KeyLookup::Opened {
                signaled: false,
                birth: None,
            } => false,
        }
    }
}

impl ThreadKey {
    /// The calling thread's key. `None` only while the thread is being torn
    /// down (its TLS is gone) or, on Windows, when its creation time cannot
    /// be read; the caller must then not record a holder it cannot check.
    pub fn current() -> Option<Self> {
        CURRENT_THREAD_KEY
            .try_with(|k| {
                if let Some(key) = k.get() {
                    return Some(key);
                }
                let key = Self::take()?;
                k.set(Some(key));
                Some(key)
            })
            .ok()
            .flatten()
    }

    #[cfg(windows)]
    fn take() -> Option<Self> {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId};
        // SAFETY: no preconditions; the pseudo handle names the caller.
        let (id, birth) = unsafe { (GetCurrentThreadId(), thread_birth(GetCurrentThread())?) };
        Self::from_parts(id, birth)
    }

    #[cfg(not(windows))]
    fn take() -> Option<Self> {
        THREAD_KEY
            .try_with(|k| Self::from_parts(k.0, 0))
            .ok()
            .flatten()
    }

    /// Key from a thread ID and the low bits of its creation time. `None`
    /// for ID 0, which no thread has.
    pub fn from_parts(id: u32, birth: u32) -> Option<Self> {
        if id == 0 {
            return None;
        }
        std::num::NonZeroU64::new(u64::from(id) | (u64::from(birth) << 32)).map(Self)
    }
    pub fn id(self) -> u32 {
        self.0.get() as u32
    }
    pub fn birth(self) -> u32 {
        (self.0.get() >> 32) as u32
    }
    /// For storing in an atomic (0 is never a key).
    pub fn to_bits(self) -> u64 {
        self.0.get()
    }
    pub fn from_bits(bits: u64) -> Option<Self> {
        std::num::NonZeroU64::new(bits).map(Self)
    }

    /// Looks the key's thread up (Windows: `OpenThread` by ID).
    pub fn lookup(self) -> KeyLookup {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{
                CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0,
            };
            use windows_sys::Win32::System::Threading::{
                OpenThread, WaitForSingleObject, THREAD_QUERY_LIMITED_INFORMATION,
                THREAD_SYNCHRONIZE,
            };
            // SAFETY: opens a handle with SYNCHRONIZE and limited query
            // access only, and closes it before returning.
            unsafe {
                let handle = OpenThread(
                    THREAD_SYNCHRONIZE | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    self.id(),
                );
                if handle.is_null() {
                    return match GetLastError() {
                        ERROR_INVALID_PARAMETER => KeyLookup::NoSuchThread,
                        _ => KeyLookup::Unknown,
                    };
                }
                let signaled = WaitForSingleObject(handle, 0) == WAIT_OBJECT_0;
                let birth = thread_birth(handle);
                CloseHandle(handle);
                KeyLookup::Opened { signaled, birth }
            }
        }
        #[cfg(not(windows))]
        {
            // Keys are never reused here: a missing key is a finished thread.
            let live = LIVE_THREAD_KEYS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&self.id());
            if live {
                KeyLookup::Opened {
                    signaled: false,
                    birth: Some(self.birth()),
                }
            } else {
                KeyLookup::NoSuchThread
            }
        }
    }

    /// Whether the thread the key was taken from has terminated. `true` is
    /// final. Unknown states (access denied) read as alive.
    pub fn has_terminated(self) -> bool {
        self.lookup().terminated(self.birth())
    }
}

thread_local! {
    static CURRENT_THREAD_KEY: std::cell::Cell<Option<ThreadKey>> =
        const { std::cell::Cell::new(None) };
}

/// Low 32 bits of a thread's creation time (`GetThreadTimes`).
#[cfg(windows)]
unsafe fn thread_birth(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<u32> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetThreadTimes;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: handle is a valid thread handle with query access (caller).
    let ok = unsafe { GetThreadTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    (ok != 0).then_some(created.dwLowDateTime)
}

#[cfg(not(windows))]
static LIVE_THREAD_KEYS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
#[cfg(not(windows))]
static NEXT_THREAD_KEY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
#[cfg(not(windows))]
struct PortableThreadId(u32);
#[cfg(not(windows))]
impl PortableThreadId {
    fn new() -> Self {
        let key = NEXT_THREAD_KEY.fetch_add(1, Ordering::Relaxed);
        LIVE_THREAD_KEYS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(key);
        Self(key)
    }
}
#[cfg(not(windows))]
impl Drop for PortableThreadId {
    fn drop(&mut self) {
        LIVE_THREAD_KEYS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|&k| k != self.0);
    }
}
#[cfg(not(windows))]
thread_local! {
    static THREAD_KEY: PortableThreadId = PortableThreadId::new();
}

impl Drop for ThreadLiveness {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            // SAFETY: the handle is owned by self.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
        }
    }
}

impl std::fmt::Debug for ThreadLiveness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadLiveness")
            .field("terminated", &self.has_terminated())
            .finish()
    }
}

#[cfg(windows)]
fn qpc_frequency() -> u64 {
    use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;
    let mut f: i64 = 0;
    // SAFETY: `f` is a writable local.
    let ok = unsafe { QueryPerformanceFrequency(&mut f) };
    if ok != 0 && f > 0 {
        f as u64
    } else {
        // QPF が失敗する環境は実質無いが、0 除算を避けるための保険。
        1_000_000_000
    }
}
#[cfg(not(windows))]
fn qpc_frequency() -> u64 {
    1_000_000_000
}
impl shirusu_transport::TimestampSource for QpcTimestampSource {
    fn now_qpc(&self) -> u64 {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Performance::QueryPerformanceCounter;
            let mut v: i64 = 0;
            // SAFETY: `v` is a writable local.
            let ok = unsafe { QueryPerformanceCounter(&mut v) };
            if ok != 0 && v > 0 {
                v as u64
            } else {
                0
            }
        }
        #[cfg(not(windows))]
        {
            shirusu_transport::TimestampSource::now_qpc(&self.fallback)
        }
    }
    fn frequency(&self) -> u64 {
        self.frequency
    }
    fn tsc_interpolation(&self) -> bool {
        self.tsc_interpolation
    }
}

#[cfg(all(windows, test))]
mod tests {
    use super::*;
    #[test]
    fn injection_handle_can_wait_for_process_exit() {
        // Missing PROCESS_SYNCHRONIZE used to make WAIT_FAILED look like
        // "still running", hanging child-tree capture after the child exited.
        let own = open_process_for_injection(std::process::id()).unwrap();
        assert!(!process_exited(own.borrow()).unwrap());
        drop(own);
        let child = spawn_suspended("cmd.exe /C exit 0").unwrap();
        let handle = open_process_for_injection(child.pid).unwrap();
        resume_main_thread(child.thread.borrow()).unwrap();
        assert_eq!(
            wait_process(child.process.borrow(), Duration::from_secs(5)),
            Some(0)
        );
        drop(child);
        assert!(process_exited(handle.borrow()).unwrap());
        // 0 はハンドルとして引き受けられない (無効なハンドルを生存と取り違えない)。
        // SAFETY: 0 is never a valid handle, so nothing is taken over.
        assert!(unsafe { OwnedHandle::from_raw(0) }.is_none());
    }

    #[test]
    fn host_liveness_handle_only_allows_waiting() {
        let remote = duplicate_self_synchronize_to_process(std::process::id()).unwrap();
        // SAFETY: 複製先はこのプロセス自身なので、値はここで有効で、ほかに
        // 閉じる者はいない。
        let handle = unsafe { OwnedHandle::from_raw(remote.raw()) }.unwrap();
        assert!(!process_exited(handle.borrow()).unwrap());
        let value = 0u8;
        assert!(
            write_process_memory(handle.borrow(), &value as *const u8 as usize, &[value]).is_err(),
            "liveness handle must not grant PROCESS_VM_WRITE"
        );
        assert!(
            remote_alloc(handle.borrow(), 4096).is_err(),
            "liveness handle must not grant PROCESS_VM_OPERATION"
        );
    }

    #[test]
    fn exit_status_preserves_live_nonzero_and_still_active_codes() {
        let own = open_process_for_injection(std::process::id()).unwrap();
        assert_eq!(process_exit_code(own.borrow()).unwrap(), None);
        drop(own);
        for code in [0, 37, 259] {
            let child = spawn_suspended(&format!("cmd.exe /C exit {code}")).unwrap();
            let handle = open_process_for_injection(child.pid).unwrap();
            resume_main_thread(child.thread.borrow()).unwrap();
            assert_eq!(
                wait_process(child.process.borrow(), Duration::from_secs(5)),
                Some(code)
            );
            drop(child);
            assert_eq!(process_exit_code(handle.borrow()).unwrap(), Some(code));
            // Querying the status must not close the borrowed handle.
            assert_eq!(process_exit_code(handle.borrow()).unwrap(), Some(code));
        }
    }

    #[test]
    fn mapping_and_event_roundtrip() {
        let m = SharedMapping::create(4096).unwrap();
        // SAFETY: the test is the only user of the mapping.
        unsafe { m.shm().producer_control() }.store(
            shirusu_transport::ProducerWord::WriteCursor,
            0xAB,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            m.shm().control().load(
                shirusu_transport::Word::WriteCursor,
                std::sync::atomic::Ordering::Relaxed
            ),
            0xAB
        );
        let e = EventObject::new(true, false).unwrap();
        assert!(e.is_auto_reset());
        assert!(!e.wait_timeout(Duration::from_millis(1)));
        e.set().unwrap();
        assert!(e.wait_timeout(Duration::from_millis(100)));
        assert!(!e.wait_timeout(Duration::from_millis(1)));
    }
    #[test]
    fn duplicate_handle_to_self_process() {
        let m = SharedMapping::create(2048).unwrap();
        let pid = std::process::id();
        let dup = m.duplicate_to_process(pid).unwrap();
        assert_ne!(dup.raw(), 0);
        assert_eq!(dup.target_pid(), pid);
        assert_eq!(dup.raw_for(pid), Some(dup.raw()));
        assert_eq!(dup.raw_for(pid ^ 4), None);
        // SAFETY: 複製先はこのプロセスで、値はほかに誰も閉じない。
        let owned = unsafe { OwnedHandle::from_raw(dup.raw()) }.unwrap();
        let again = SharedMapping::from_owned(owned, 2048).unwrap();
        assert!(
            SharedMapping::from_owned(
                // SAFETY: 同上 (もう 1 本複製する)。
                unsafe { OwnedHandle::from_raw(m.duplicate_to_process(pid).unwrap().raw()) }
                    .unwrap(),
                1 << 20
            )
            .is_err(),
            "a view larger than the section is refused"
        );
        // SAFETY: the test is the only user of the mapping.
        unsafe { again.shm().producer_control() }.store(
            shirusu_transport::ProducerWord::WriteCursor,
            7,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            m.shm().control().load(
                shirusu_transport::Word::WriteCursor,
                std::sync::atomic::Ordering::Relaxed
            ),
            7
        );
    }
    #[test]
    fn restricted_mapping_roundtrip() {
        // 現在のユーザーだけに開ける DACL 付き Mapping が作れて使えること。
        let m = SharedMapping::create_restricted(4096).unwrap();
        // SAFETY: the test is the only user of the mapping.
        unsafe { m.shm().producer_control() }.store(
            shirusu_transport::ProducerWord::WriteCursor,
            0x5A,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            m.shm().control().load(
                shirusu_transport::Word::WriteCursor,
                std::sync::atomic::Ordering::Relaxed
            ),
            0x5A
        );
        assert_eq!(m.len(), 4096);
    }
    #[test]
    fn duplicate_event_to_self_process_signals_across_handle() {
        // A68 の DuplicateHandle 経路: 自プロセスへ複製したハンドルから作った
        // EventObject で、元の EventObject の SetEvent を観測できること。
        let e = EventObject::new(true, false).unwrap();
        let dup_value = e.duplicate_to_process(std::process::id()).unwrap();
        assert_ne!(dup_value.raw(), 0);
        // SAFETY: 複製先はこのプロセスで、値はほかに誰も閉じない。
        let dup = EventObject::from_owned(
            unsafe { OwnedHandle::from_raw(dup_value.raw()) }.unwrap(),
            true,
        );
        assert!(!dup.wait_timeout(Duration::from_millis(1)));
        e.set().unwrap();
        assert!(dup.wait_timeout(Duration::from_millis(100)));
        // auto-reset: 複製側の待機がシグナルを消費したので、元側ももう立っていない。
        assert!(!e.wait_timeout(Duration::from_millis(1)));
    }
    #[test]
    fn duplicate_ring_notify_space_to_self_process() {
        // レビュー第5回 #1 の対側: RingNotify の space イベントも自プロセスへ
        // 複製でき、非ゼロのハンドル値が返ること (スタブ版テストの Windows 側)。
        let notify = RingNotify::new().unwrap();
        let dup = notify
            .duplicate_space_to_process(std::process::id())
            .unwrap();
        assert_ne!(dup.raw(), 0);
    }
}

#[cfg(test)]
mod portable_tests {
    use super::*;

    /// P1-2: a lookup counts as termination only on proof. Every
    /// combination of the lookup outcome is listed (no catch-all).
    #[test]
    fn key_lookup_decides_termination_only_on_proof() {
        let k = 0x1234_5678;
        let table = [
            (KeyLookup::NoSuchThread, true),
            // Access denied and friends: the thread may well be alive.
            (KeyLookup::Unknown, false),
            (
                KeyLookup::Opened {
                    signaled: true,
                    birth: Some(k),
                },
                true,
            ),
            (
                KeyLookup::Opened {
                    signaled: true,
                    birth: None,
                },
                true,
            ),
            (
                KeyLookup::Opened {
                    signaled: false,
                    birth: Some(k),
                },
                false,
            ),
            // Same ID, later thread: the keyed thread is gone.
            (
                KeyLookup::Opened {
                    signaled: false,
                    birth: Some(k ^ 1),
                },
                true,
            ),
            (
                KeyLookup::Opened {
                    signaled: false,
                    birth: None,
                },
                false,
            ),
        ];
        for (lookup, terminated) in table {
            assert_eq!(lookup.terminated(k), terminated, "{lookup:?}");
        }
    }

    #[test]
    fn thread_key_round_trips_and_is_never_zero() {
        assert_eq!(ThreadKey::from_parts(0, 5), None);
        let key = ThreadKey::from_parts(8, 0xdead_beef).unwrap();
        assert_eq!((key.id(), key.birth()), (8, 0xdead_beef));
        assert_eq!(ThreadKey::from_bits(key.to_bits()), Some(key));
        assert_eq!(ThreadKey::from_bits(0), None);
    }

    /// The calling thread is alive; a thread that finished is terminated;
    /// the key is stable within a thread.
    #[test]
    fn thread_key_tracks_real_threads() {
        let me = ThreadKey::current().unwrap();
        assert_eq!(ThreadKey::current(), Some(me));
        assert!(!me.has_terminated());
        let other = std::thread::spawn(|| ThreadKey::current().unwrap())
            .join()
            .unwrap();
        assert_ne!(other, me);
        // Windows: the thread object may linger until its last handle closes;
        // it is signaled (or gone) either way once join returned.
        assert!(other.has_terminated());
    }

    /// Windows: a key whose ID is a live thread's but whose creation time is
    /// not (the ID was reused) reads as terminated.
    #[cfg(windows)]
    #[test]
    fn reused_thread_id_reads_as_terminated() {
        let me = ThreadKey::current().unwrap();
        let forged = ThreadKey::from_parts(me.id(), me.birth() ^ 1).unwrap();
        assert!(forged.has_terminated());
        assert!(!me.has_terminated());
    }
    #[test]
    fn positive_waits_round_up_without_using_infinite() {
        assert_eq!(wait_timeout_millis(Duration::ZERO), 0);
        assert_eq!(wait_timeout_millis(Duration::from_nanos(1)), 1);
        assert_eq!(wait_timeout_millis(Duration::from_micros(999)), 1);
        assert_eq!(wait_timeout_millis(Duration::from_millis(1)), 1);
        assert_eq!(wait_timeout_millis(Duration::from_micros(1001)), 2);
        assert_eq!(wait_timeout_millis(Duration::MAX), u32::MAX - 1);
    }
    #[test]
    fn qpc_timestamp_source_is_monotonic_with_stable_frequency() {
        use shirusu_transport::TimestampSource as _;
        let src = QpcTimestampSource::new();
        let freq = src.frequency();
        assert!(freq > 0);
        assert_eq!(src.frequency(), freq, "frequency must be stable");
        let a = src.now_qpc();
        let b = src.now_qpc();
        assert!(b >= a, "QPC stamps must be monotonic: {b} < {a}");
        // 別のインスタンスでも同じ時刻系 (プロセス間で比較可能な前提)。
        let other = QpcTimestampSource::new();
        assert_eq!(other.frequency(), freq);
        assert!(other.now_qpc() >= a);
    }
    #[test]
    fn wait_with_recheck_recovers_missed_notification() {
        let notify = ProcessNotify::new().unwrap();
        let mut calls = 0;
        let found = notify.wait_with_recheck(
            ReCheckPolicy::default(),
            || {
                calls += 1;
                calls >= 3
            },
            Duration::from_millis(50),
        );
        assert!(found);
    }
    #[test]
    fn wait_with_recheck_does_not_busy_loop() {
        // R4: 累積経過時間を ReCheckPolicy::wait_timeout (interval - waited) に
        // 渡すと、2ms 経過後はスライスが 0 になり、ゼロ待機のまま
        // total_timeout まで再確認だけが猛烈に回る (ビジーループ)。
        // 修正後は反復ごとに interval を上限に待つため、再確認は
        // 約 total/interval 回 (100ms / 2ms = 約 50 回) に収まる。
        let notify = ProcessNotify::new().unwrap();
        let calls = AtomicU64::new(0);
        let start = std::time::Instant::now();
        let found = notify.wait_with_recheck(
            ReCheckPolicy::default(),
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                false
            },
            Duration::from_millis(100),
        );
        assert!(!found, "no data and no notification -> false after timeout");
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "must wait out the total timeout"
        );
        let n = calls.load(Ordering::Relaxed);
        assert!(n < 200, "recheck called {n} times in 100ms: busy loop");
    }
    #[test]
    fn stub_mapping_has_requested_len() {
        let m = SharedMapping::create(4096).unwrap();
        assert_eq!(m.len(), 4096);
    }

    /// L05: プロセス一覧の列挙。Windows では自プロセスが含まれ、
    /// Windows 以外では Unsupported を返す（スタブ）。
    #[test]
    fn enumerate_processes_lists_self_on_windows() {
        let list = enumerate_processes();
        #[cfg(windows)]
        {
            let list = list.expect("enumerate_processes on windows");
            let me = std::process::id();
            assert!(
                list.iter().any(|(pid, _)| *pid == me),
                "own pid {me} must be in the process list"
            );
            assert!(!list.is_empty());
        }
        #[cfg(not(windows))]
        {
            assert!(matches!(list, Err(WinError::Unsupported)));
        }
    }
    /// レビュー 2026-11-16 P2: 値は複製先のプロセスでしか数値にならない。
    #[test]
    fn remote_handle_belongs_to_its_target_process() {
        let h = RemoteHandle {
            raw: 0x44,
            target_pid: 7,
        };
        assert_eq!(h.target_pid(), 7);
        assert_eq!(h.raw_for(7), Some(0x44));
        assert_eq!(h.raw_for(8), None);
    }
    #[test]
    fn notify_space_only_when_writer_waiting() {
        let cfg = shirusu_transport::RingConfig::new(8, 256);
        let region = shirusu_transport::ring::SharedRegion::new(cfg, 1, 1).unwrap();
        let notify = RingNotify::new().unwrap();
        assert!(!notify_space_if_writer_waiting(region.control(), &notify).unwrap());
        // SAFETY: the test is the only user of the mapping.
        unsafe { region.producer_control() }.store(
            shirusu_transport::ProducerWord::WriterWaiting,
            1,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert!(notify_space_if_writer_waiting(region.control(), &notify).unwrap());
    }
    #[test]
    fn event_clock_wakes_on_space_available() {
        use shirusu_transport::Clock;
        let notify = Arc::new(RingNotify::new().unwrap());
        let clock = EventClock::new(Arc::clone(&notify));
        // イベントが立っていなければタイムアウトまで待つ（即復帰しない）。
        let start = std::time::Instant::now();
        clock.sleep(Duration::from_millis(20));
        assert!(start.elapsed() >= Duration::from_millis(5));
        // イベントを立てれば、長い待機でも即復帰する。
        notify.notify_space().unwrap();
        let start = std::time::Instant::now();
        clock.sleep(Duration::from_millis(200));
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(clock.sleep_calls(), 2);
    }
    #[cfg(not(windows))]
    #[test]
    fn duplicate_space_to_process_is_unsupported_on_stub() {
        // Linux スタブにプロセス間の実体は無い。Windows 実機では
        // `duplicate_event_to_self_process_signals_across_handle` と
        // `duplicate_ring_notify_space_to_self_process` が通ること。
        // (レビュー第5回 #1: cfg が無いと Windows 実機で Ok が返り失敗する)
        let notify = RingNotify::new().unwrap();
        assert_eq!(
            notify.duplicate_space_to_process(std::process::id()),
            Err(WinError::Unsupported)
        );
    }
    #[test]
    fn mapping_create_shorter_than_control_is_invalid_argument() {
        // Windows 実装でも Control が入らない長さは CreateFileMappingW の前に弾く
        for len in [0, 1, shirusu_transport::layout::CONTROL_SIZE - 1] {
            assert!(matches!(
                SharedMapping::create(len),
                Err(WinError::InvalidArgument("len"))
            ));
        }
        assert!(SharedMapping::create(shirusu_transport::layout::CONTROL_SIZE).is_ok());
    }
    #[test]
    fn win_error_display_strings() {
        assert_eq!(
            WinError::Unsupported.to_string(),
            "not supported on this platform"
        );
        assert_eq!(
            WinError::OsError {
                api: "CreateEventW",
                code: 5
            }
            .to_string(),
            "CreateEventW failed with OS error 5"
        );
        assert_eq!(
            WinError::InvalidArgument("len").to_string(),
            "invalid argument: len"
        );
    }
    #[cfg(not(windows))]
    #[test]
    fn stub_event_manual_reset_and_initially_signaled() {
        let e = EventObject::new(false, true).unwrap();
        assert!(!e.is_auto_reset());
        assert!(
            e.wait_timeout(Duration::from_millis(10)),
            "initially signaled"
        );
        // スタブの wait は auto/manual を区別せずシグナルを消費する
        // (Windows 実機の manual-reset とは違う、スタブの割り切り)。
        assert!(!e.wait_timeout(Duration::from_millis(1)));
        e.set().unwrap();
        assert!(e.wait_timeout(Duration::from_millis(10)));
    }
    #[test]
    fn event_clock_ticks_and_monotonicity() {
        use shirusu_transport::Clock;
        let notify = Arc::new(RingNotify::new().unwrap());
        let clock = EventClock::new(notify);
        assert_eq!(clock.ticks_per_second(), 1_000_000_000);
        let a = clock.now_ticks();
        let b = clock.now_ticks();
        assert!(b >= a);
        assert_eq!(clock.sleep_calls(), 0);
    }
    #[test]
    fn process_notify_recheck_timeout_returns_false() {
        let notify = ProcessNotify::new().unwrap();
        let start = std::time::Instant::now();
        let found = notify.wait_with_recheck(
            ReCheckPolicy::default(),
            || false,
            Duration::from_millis(30),
        );
        assert!(!found, "no data and no notification -> false after timeout");
        assert!(start.elapsed() >= Duration::from_millis(25));
    }
    #[test]
    fn process_notify_set_event_wakes_wait() {
        let notify = ProcessNotify::new().unwrap();
        notify.notify_data_ready().unwrap();
        // data_ready を消費する待機が成功すれば recheck が呼ばれる
        let found =
            notify.wait_with_recheck(ReCheckPolicy::default(), || true, Duration::from_millis(10));
        assert!(found);
    }
    #[cfg(not(windows))]
    #[test]
    fn stub_process_operations_are_unsupported() {
        let m = SharedMapping::create(4096).unwrap();
        assert!(matches!(
            SharedMapping::create_restricted(4096),
            Err(WinError::Unsupported)
        ));
        // スタブでは閉じない値なので、任意の数を引き受けてよい。
        // SAFETY: the stub never closes handle values, so a made-up value is harmless.
        let fake = || unsafe { OwnedHandle::from_raw(123) }.unwrap();
        assert!(matches!(
            SharedMapping::from_owned(fake(), 4096),
            Err(WinError::Unsupported)
        ));
        assert!(!m.is_empty());
        assert!(matches!(
            m.duplicate_to_process(1),
            Err(WinError::Unsupported)
        ));
        assert!(matches!(m.make_inheritable(), Err(WinError::Unsupported)));
        let e = EventObject::new(true, false).unwrap();
        assert!(matches!(
            e.duplicate_to_process(1),
            Err(WinError::Unsupported)
        ));
        assert!(matches!(e.make_inheritable(), Err(WinError::Unsupported)));
        assert!(matches!(
            spawn_inheriting("true"),
            Err(WinError::Unsupported)
        ));
        let handle = fake();
        assert_eq!(
            wait_process(handle.borrow(), Duration::from_millis(1)),
            None
        );
        // ハンドルから作ったスタブイベントは独立した新しいイベントとして動く
        let stub = EventObject::from_owned(fake(), true);
        assert!(!stub.wait_timeout(Duration::from_millis(1)));
        stub.set().unwrap();
        assert!(stub.wait_timeout(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod path_tests {
    use super::{is_absolute_windows_path, same_windows_path};

    #[test]
    fn only_fully_qualified_paths_are_absolute() {
        for p in [
            r"C:\a\agent.dll",
            "c:/a/agent.dll",
            r"\\server\share\agent.dll",
        ] {
            assert!(is_absolute_windows_path(p), "{p}");
        }
        for p in [
            "agent.dll",
            r".\agent.dll",
            r"C:agent.dll",
            r"\agent.dll",
            "",
        ] {
            assert!(!is_absolute_windows_path(p), "{p}");
        }
    }

    #[test]
    fn module_paths_compare_full_path_not_file_name() {
        assert!(same_windows_path(
            r"C:\Tools\Agent.DLL",
            "c:/tools/agent.dll"
        ));
        assert!(!same_windows_path(
            r"C:\App\agent.dll",
            r"C:\Tools\agent.dll"
        ));
    }
}

/// Whether the calling thread owns the process Loader Lock right now.
///
/// Reads `PEB.LoaderLock` (the `RTL_CRITICAL_SECTION` the loader holds while
/// it runs `DllMain` and TLS callbacks) and compares its `OwningThread` with
/// the thread id in the TEB. Only the owner can see its own id there, so a
/// `false` answer is exact for the calling thread. Returns `true` (held, do
/// not wait) when the lock pointer is missing and on non-Windows builds.
///
/// Called by a Ring writer only before it would wait for space, never on the
/// normal write path.
#[inline(never)]
pub fn loader_lock_held_by_current_thread() -> bool {
    #[cfg(all(windows, target_arch = "x86_64"))]
    // SAFETY: gs:[0x60] is the PEB and gs:[0x48] ClientId.UniqueThread on
    // x64 Windows; PEB+0x110 is LoaderLock; OwningThread is at +0x10.
    unsafe {
        let peb: *const u8;
        let tid: usize;
        core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, preserves_flags, readonly));
        core::arch::asm!("mov {}, gs:[0x48]", out(reg) tid, options(nostack, preserves_flags, readonly));
        let cs = core::ptr::read_volatile(peb.add(0x110) as *const *const u8);
        if cs.is_null() {
            return true;
        }
        core::ptr::read_volatile(cs.add(0x10) as *const usize) == tid
    }
    #[cfg(all(windows, target_arch = "x86"))]
    // SAFETY: fs:[0x30] is the PEB and fs:[0x24] ClientId.UniqueThread on
    // x86 (and WOW64) Windows; PEB+0xA0 is LoaderLock; OwningThread at +0x0C.
    unsafe {
        let peb: *const u8;
        let tid: usize;
        core::arch::asm!("mov {}, fs:[0x30]", out(reg) peb, options(nostack, preserves_flags, readonly));
        core::arch::asm!("mov {}, fs:[0x24]", out(reg) tid, options(nostack, preserves_flags, readonly));
        let cs = core::ptr::read_volatile(peb.add(0xA0) as *const *const u8);
        if cs.is_null() {
            return true;
        }
        core::ptr::read_volatile(cs.add(0x0C) as *const usize) == tid
    }
    #[cfg(not(all(windows, any(target_arch = "x86_64", target_arch = "x86"))))]
    {
        true
    }
}

#[cfg(all(test, windows))]
mod precise_wait_tests {
    use std::time::{Duration, Instant};

    /// P2-14: a 1ms wait does not stretch to the default timer period.
    /// The bound is loose (CI machines are noisy); the point is that 20 waits
    /// take well under 20 default periods (~312ms).
    #[test]
    fn short_waits_use_a_high_resolution_timer() {
        let event = super::EventObject::new(false, false).unwrap();
        let started = Instant::now();
        for _ in 0..20 {
            assert!(!event.wait_timeout(Duration::from_millis(1)));
        }
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(200), "{elapsed:?}");
        event.set().unwrap();
        assert!(event.wait_timeout(Duration::from_millis(1)));
    }
}

#[cfg(all(test, windows))]
mod loader_lock_tests {
    #[test]
    fn ordinary_thread_does_not_hold_loader_lock() {
        assert!(!super::loader_lock_held_by_current_thread());
        let other = std::thread::spawn(super::loader_lock_held_by_current_thread);
        assert!(!other.join().unwrap());
    }
}

#[cfg(test)]
mod stop_request_tests {
    /// P1-5: 停止要求はプロセス全体の印として残る。
    #[test]
    fn stop_request_is_sticky() {
        super::request_stop();
        assert!(super::stop_requested());
        super::notify_stop_finished();
        assert!(super::stop_requested());
    }
}
