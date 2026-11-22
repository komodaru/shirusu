//! 共有メモリ領域の型 (レビュー 2026-11-15 P0-1 / P0-3)。
//!
//! 生ポインタと任意オフセットで共有メモリへ触る安全な関数は置かない。
//!
//! - [`Backing`]: 領域の実体 (ヒープ確保、Windows の File Mapping)。
//!   実装は `unsafe impl` で「先頭が 8 バイト境界、`len` バイトが自分の
//!   存続中有効」を約束する。
//! - [`Shm`]: 検証済みの領域。実体を `Arc` で共有するので、Writer / Reader
//!   が領域より長生きしても解放済みメモリへ触れない (旧 API は「Mapping が
//!   Writer より長生きする」ことを呼び出し側に任せていた)。
//! - [`Control`]: 先頭 256 バイトの Control を `&[AtomicU64; 32]` として
//!   借用する。語は閉じた列挙 [`Word`] でしか指せず、領域外・非整列の
//!   アクセスは書けない。
//! - 共有領域を `&[u8]` として公開しない。中身が欲しいときは
//!   [`Shm::snapshot`] / [`Shm::copy_out`] で所有データ (`Vec<u8>`) に写す。
//!   どちらも生コピーなので `unsafe` (同時に書く者がいないことを約束する)。
//!   `copy_out` は Control に掛かる範囲を拒む。Control は原子読みの
//!   [`Control::snapshot`] でしか写さない (レビュー 2026-11-16 P0)。
//! - Control の書き込みは役割で分ける (レビュー 2026-11-16 P1):
//!   [`Control`] は読み取り専用、[`ProducerControl`] は書き手の語
//!   ([`ProducerWord`])、[`ConsumerControl`] は読み手の語
//!   ([`ConsumerWord`])、Meta 行の初期化は [`UninitializedRing::publish`]
//!   だけ。

use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::layout::{RingConfig, CONTROL_SIZE, LAYOUT_VERSION, MAGIC};

/// Control の 64 bit 語の数。
pub const CONTROL_WORDS: usize = CONTROL_SIZE / 8;
// Shared by every view of a Mapping. It survives Ring generation changes.
const SLOT_REGISTRATION_WORD: usize = 208 / 8;
/// 読み手 (Host の回収) の登録権。Mapping ごとに読み手は一つ (7c5a553 レビュー P2-2: 同じ
/// 共有領域を二度登録すると、二つの回収が同じイベントを最初から読み、2 件が 4 件として
/// 保存された)。Slot の登録権と同じく、Ring の世代交代 (Control の再初期化) をまたいで保つ。
const READER_REGISTRATION_WORD: usize = 216 / 8;
const _: () = assert!(READER_REGISTRATION_WORD != SLOT_REGISTRATION_WORD);
const _: () = assert!(CONTROL_WORDS * 8 == CONTROL_SIZE);

/// 共有領域の実体。
///
/// # Safety
/// 実装は次を保証すること。
/// - `base()` は null でなく 8 バイト境界にあり、毎回同じ値を返す。
/// - `[base, base + len)` は `self` が生きている間ずっと読み書きでき、
///   Rust の参照 (`&[u8]` / `&mut [u8]`) の対象にはならない。
/// - 同じメモリへほかのプロセスやスレッドが触るのは、原子操作か、
///   Ring の公開規約 (確定前の範囲だけを書き、確定済みの範囲だけを読む) に
///   従った生コピーに限る。
pub unsafe trait Backing: Send + Sync + 'static {
    fn base(&self) -> NonNull<u8>;
    fn byte_len(&self) -> usize;
}

/// [`Shm`] を作れなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmError {
    /// Control すら入らない。
    TooSmall { len: usize },
    /// 先頭が 8 バイト境界にない。
    Misaligned,
    /// ヒープ確保に失敗した (OOM)。対象プロセスを止めずに呼び出し側へ返す。
    AllocFailed { len: usize },
    /// 設定の必要容量より領域が小さい。
    ShorterThanConfig { len: usize, required: usize },
}

impl std::fmt::Display for ShmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooSmall { len } => {
                write!(f, "shared region of {len} bytes has no room for Control")
            }
            Self::Misaligned => f.write_str("shared region is not 8-byte aligned"),
            Self::AllocFailed { len } => write!(f, "allocating a {len}-byte shared region failed"),
            Self::ShorterThanConfig { len, required } => {
                write!(
                    f,
                    "shared region of {len} bytes is shorter than the ring ({required})"
                )
            }
        }
    }
}
impl std::error::Error for ShmError {}

/// ヒープ上の共有領域 (プロセス内の初期 Slot、テスト、ベンチ)。
struct HeapBacking {
    ptr: NonNull<u8>,
    layout: std::alloc::Layout,
}
// SAFETY: 中身へは Shm 経由 (原子操作・Ring 規約の生コピー) でしか触れない。
unsafe impl Send for HeapBacking {}
// SAFETY: 同上。
unsafe impl Sync for HeapBacking {}
// SAFETY: 64 バイト整列で確保し、Drop まで解放しない。参照は作らない。
unsafe impl Backing for HeapBacking {
    fn base(&self) -> NonNull<u8> {
        self.ptr
    }
    fn byte_len(&self) -> usize {
        self.layout.size()
    }
}
impl Drop for HeapBacking {
    fn drop(&mut self) {
        // SAFETY: ptr は alloc_zeroed で同じ layout により確保した値。
        // Shm が Arc で共有するので、最後の利用者が消えた後にだけ呼ばれる。
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// 検証済みの共有領域。複製は実体の参照を増やすだけ。
#[derive(Clone)]
pub struct Shm {
    keep: Arc<dyn Backing>,
    base: NonNull<u8>,
    len: usize,
}
// SAFETY: 実体は Backing: Send + Sync。base はその実体のアドレスの写し。
unsafe impl Send for Shm {}
// SAFETY: 同上。&Shm から得られる操作は原子操作か所有データへのコピー。
unsafe impl Sync for Shm {}

impl std::fmt::Debug for Shm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shm")
            .field("base", &self.base)
            .field("len", &self.len)
            .finish()
    }
}

/// A reservation rolls back on drop until the mapping is bound to a slot.
/// ```compile_fail
/// fn twice(reservation: shirusu_transport::shm::SlotRegistration) {
///     reservation.bind();
///     reservation.bind();
/// }
/// ```
/// ```compile_fail,E0277
/// fn require<T: Clone>() {}
/// require::<shirusu_transport::shm::SlotRegistration>();
/// ```
pub struct SlotRegistration {
    region: Shm,
    /// Set by [`bind`](Self::bind) (which consumes the reservation): the drop that follows keeps
    /// the registration word.
    bound: bool,
}

impl SlotRegistration {
    /// A registered slot reuses the same mapping across writer generations.
    pub fn bind(mut self) -> Shm {
        self.bound = true;
        self.region.clone()
    }
}

impl Drop for SlotRegistration {
    fn drop(&mut self) {
        if !self.bound {
            self.region.control().words[SLOT_REGISTRATION_WORD].store(0, Ordering::Release);
        }
    }
}

/// 読み手の登録権。持っている間、この Mapping (別のビューを含む) を読む回収は自分だけ。
/// 手放す (drop する) と、別の読み手が登録できる。
/// ```compile_fail,E0277
/// fn require<T: Clone>() {}
/// require::<shirusu_transport::shm::ReaderRegistration>();
/// ```
pub struct ReaderRegistration {
    region: Shm,
}

impl ReaderRegistration {
    /// 登録した領域
    pub fn shm(&self) -> &Shm {
        &self.region
    }
}

impl std::fmt::Debug for ReaderRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ReaderRegistration").field(&self.region).finish()
    }
}

impl Drop for ReaderRegistration {
    fn drop(&mut self) {
        self.region.control().words[READER_REGISTRATION_WORD].store(0, Ordering::Release);
    }
}

/// [`Shm::reserve_reader`] が登録を拒んだ理由
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveReaderError {
    /// 既に読み手がいる (同じ実体の別ビューからの登録を含む)
    Busy,
    /// 今の incarnation は前の読み手が途中まで読んでいる (ReadCursor が 0 でない)。読み手は
    /// 位置 0 から読むので、登録すれば読み終えたイベントを再び記録するか、残りを損失と誤る
    /// (d4fc3c0 レビュー 1.2)。途中からの再開は提供しない: Agent が Slot を新しい incarnation へ
    /// 初期化し直せば (ReadCursor が 0 に戻れば) 登録できる。
    AlreadyRead { read_cursor: u64 },
}

impl Shm {
    /// この Mapping の、今の incarnation を位置 0 から読む読み手として登録する。既に読み手が
    /// いれば Busy、前の読み手が途中まで読んだ incarnation なら AlreadyRead。
    /// 持っている登録権は「新しい incarnation を読む権利」で、前の位置からの再開ではない。
    pub fn reserve_reader(&self) -> Result<ReaderRegistration, ReserveReaderError> {
        self.control().words[READER_REGISTRATION_WORD]
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ReserveReaderError::Busy)?;
        let registration = ReaderRegistration {
            region: self.clone(),
        };
        // 読み手は自分だけになったので、ReadCursor を動かすものはない (書き手は読まない)
        let read_cursor = self.control().load(Word::ReadCursor, Ordering::Acquire);
        if read_cursor != 0 {
            // 登録は drop で取り消される
            return Err(ReserveReaderError::AlreadyRead { read_cursor });
        }
        Ok(registration)
    }

    /// 実体を検証して包む。
    pub fn new(backing: Arc<dyn Backing>) -> Result<Self, ShmError> {
        let base = backing.base();
        let len = backing.byte_len();
        if !(base.as_ptr() as usize).is_multiple_of(std::mem::align_of::<AtomicU64>()) {
            return Err(ShmError::Misaligned);
        }
        if len < CONTROL_SIZE {
            return Err(ShmError::TooSmall { len });
        }
        Ok(Self {
            keep: backing,
            base,
            len,
        })
    }

    /// ゼロ初期化したヒープ領域。確保失敗は Err (panic しない)。
    pub fn alloc_zeroed(len: usize) -> Result<Self, ShmError> {
        if len < CONTROL_SIZE {
            return Err(ShmError::TooSmall { len });
        }
        let layout = std::alloc::Layout::from_size_align(len, crate::layout::CACHE_LINE)
            .map_err(|_| ShmError::AllocFailed { len })?;
        // SAFETY: layout の大きさは CONTROL_SIZE 以上で 0 ではない。
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or(ShmError::AllocFailed { len })?;
        Self::new(Arc::new(HeapBacking { ptr, layout }))
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 先頭 256 バイトの Control (構築時に長さと整列を確かめてある)。
    pub fn control(&self) -> Control<'_> {
        // SAFETY: base は 8 バイト境界、len >= CONTROL_SIZE、実体は self が
        // 生きている間有効で、Control の語は全員が原子操作でだけ触る。
        let words = unsafe { &*(self.base.as_ptr() as *const [AtomicU64; CONTROL_WORDS]) };
        Control { words }
    }

    /// 領域が `cfg` の Ring を収められるか確かめる。
    pub fn check_fits(&self, cfg: RingConfig) -> Result<(), ShmError> {
        if self.len < cfg.required_len() {
            return Err(ShmError::ShorterThanConfig {
                len: self.len,
                required: cfg.required_len(),
            });
        }
        Ok(())
    }

    /// 先頭アドレス。参照外し (読み書き) は `unsafe` が要る。Ring の生コピー
    /// (確定前の範囲への書き込み、確定済みの範囲の読み出し) だけが使う。
    pub fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// Reserve this mapping for exactly one slot, including across mapped views.
    pub fn reserve_slot(&self) -> Option<SlotRegistration> {
        self.control().words[SLOT_REGISTRATION_WORD]
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(SlotRegistration {
            region: self.clone(),
            bound: false,
        })
    }

    /// 同じ実体かどうか。
    pub fn same_region(&self, other: &Shm) -> bool {
        Arc::ptr_eq(&self.keep, &other.keep)
    }

    /// `range` のバイトを所有データへ生コピーで写す。範囲外と、Control
    /// (先頭 [`CONTROL_SIZE`] バイト) に掛かる範囲は None。Control は
    /// 原子操作でしか触れないので [`Control::snapshot`] を使う。
    ///
    /// # Safety
    /// 写している間、`range` へ他のスレッドが書き込まないこと。Ring では
    /// 読み手が確定済み (書き手が書き終えて Cursor を Release で公開し、
    /// 読み手が Acquire で読んだ) の範囲だけを写せばこれを満たす。
    /// レビュー 2026-11-16 P0: 旧版は safe で、`Control` の原子書き込みや
    /// 書き手の生コピーと同時に呼べた (safe Rust 同士のデータ競合)。
    pub unsafe fn copy_out(&self, range: Range<usize>) -> Option<Vec<u8>> {
        if range.start < CONTROL_SIZE || range.start > range.end || range.end > self.len {
            return None;
        }
        let mut out = vec![0u8; range.end - range.start];
        // SAFETY: 範囲は Control より後ろの領域内。写し先は新しい Vec で
        // 重ならない。参照は作らず、同時の書き込みが無いことは呼び出し側の約束。
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.as_ptr().add(range.start),
                out.as_mut_ptr(),
                out.len(),
            );
        }
        Some(out)
    }

    /// 領域全体の写し。Control は語ごとの原子読みで、残りは生コピーで写す。
    ///
    /// # Safety
    /// 写している間、Control より後ろ (Descriptor / Payload) へ他のスレッドが
    /// 書き込まないこと (書き手が止まっているテスト・検証器の像づくり)。
    pub unsafe fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        out.extend_from_slice(&self.control().snapshot());
        // SAFETY: 呼び出し側の約束をそのまま渡す。範囲は Control の直後から末尾。
        if let Some(rest) = unsafe { self.copy_out(CONTROL_SIZE..self.len) } {
            out.extend_from_slice(&rest);
        }
        out
    }

    /// 書き手 (Producer) 側の Control。
    ///
    /// # Safety
    /// 呼び出し側はこの Ring の書き手側 (Writer、または書き手を持つ Agent)
    /// であること。Producer の語 (WriteCursor / PayloadWritePosition など) は
    /// 読み手が生コピーする範囲を決めるので、SPSC の規約 (確定した範囲だけを
    /// 公開する) から外れた値を書くと、読み手のコピーと書き手の書き込みが同じ
    /// バイトで重なる。
    pub unsafe fn producer_control(&self) -> ProducerControl<'_> {
        ProducerControl {
            ctl: self.control(),
        }
    }

    /// 読み手 (Consumer) 側の Control。
    ///
    /// # Safety
    /// 呼び出し側はこの Ring の読み手側 (Host の回収経路) であること。
    /// ReadCursor / PayloadReadPosition は書き手が上書きしてよい範囲を決める
    /// ので、まだ写していない位置まで進めると、書き手の書き込みと読み手の
    /// コピーが同じバイトで重なる。
    pub unsafe fn consumer_control(&self) -> ConsumerControl<'_> {
        ConsumerControl {
            ctl: self.control(),
        }
    }
}

/// 生成直後で、まだ誰も使っていない Ring 領域 (レビュー 2026-11-16 P1)。
/// Control の初期化 (Magic・版数・容量・Generation・Owner) はこの型を
/// 消費する [`UninitializedRing::publish`] でしかできない。稼働中の Ring の
/// Control を初期化し直すコードは書けない。
pub struct UninitializedRing {
    shm: Shm,
    cfg: RingConfig,
}

impl UninitializedRing {
    /// ゼロ初期化したヒープ領域 (誰とも共有していない)。
    pub fn alloc(cfg: RingConfig) -> Result<Self, ShmError> {
        let shm = Shm::alloc_zeroed(cfg.required_len())?;
        Ok(Self { shm, cfg })
    }

    /// 既存の領域を、これから初期化する Ring として扱う。
    ///
    /// # Safety
    /// `shm` の Ring をいま読み書きしている者がいないこと (新しく作った、
    /// あるいは前の書き手と読み手が手放した Mapping)。
    pub unsafe fn from_shm(shm: Shm, cfg: RingConfig) -> Result<Self, ShmError> {
        shm.check_fits(cfg)?;
        Ok(Self { shm, cfg })
    }

    pub fn config(&self) -> RingConfig {
        self.cfg
    }

    /// Magic 以外の Control を書く (公開前の段階を観測するテスト用)。
    /// 何度呼んでもよく、公開は [`UninitializedRing::publish`] が行う。
    pub fn prepare(&self, generation: u64, owner_thread_id: u64) {
        init_fields(self.shm.control(), self.cfg, generation, owner_thread_id);
    }

    /// Control を初期化して公開する。
    ///
    /// 公開順序 (レビュー第5回 #4): Magic は**最後**に `Release` で書く。
    /// それ以外の全フィールド (ゼロクリア・版数・容量・Generation・Owner) を
    /// 先に書き終えてから Magic を公開することで、Magic を `Acquire` で読んだ
    /// Host は初期化済みの全フィールドを見る。
    pub fn publish(self, generation: u64, owner_thread_id: u64) -> Shm {
        init_control(self.shm.control(), self.cfg, generation, owner_thread_id);
        self.shm
    }
}

fn init_control(ctl: Control<'_>, cfg: RingConfig, generation: u64, owner_thread_id: u64) {
    init_fields(ctl, cfg, generation, owner_thread_id);
    ctl.at(Word::Magic).store(MAGIC, Ordering::Release);
}

fn init_fields(ctl: Control<'_>, cfg: RingConfig, generation: u64, owner_thread_id: u64) {
    for (index, w) in ctl.words.iter().enumerate() {
        if index != SLOT_REGISTRATION_WORD && index != READER_REGISTRATION_WORD {
            w.store(0, Ordering::Relaxed);
        }
    }
    let set = |w: Word, v: u64| ctl.at(w).store(v, Ordering::Relaxed);
    set(Word::LayoutVersion, LAYOUT_VERSION);
    set(Word::DescriptorCapacity, cfg.descriptor_capacity_u64());
    set(Word::PayloadCapacity, cfg.payload_capacity_u64());
    set(Word::Generation, generation);
    set(Word::OwnerThreadId, owner_thread_id);
}

/// Control 領域の語。閉じた集合で、オフセットはここでしか決まらない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Word {
    // --- Writer line (0..64): 書き手だけが更新する ---
    WriteCursor,
    PayloadWritePosition,
    DroppedEvents,
    DroppedPayloadBytes,
    DegradedEvents,
    OverflowState,
    ExpansionRequested,
    LastWaitNs,
    // --- Reader line (64..128): 読み手だけが更新する ---
    ReadCursor,
    PayloadReadPosition,
    HostWaiting,
    RingStopped,
    HostDrainCount,
    // --- Meta line (128..192): 生成時に一度だけ書く ---
    Magic,
    LayoutVersion,
    DescriptorCapacity,
    PayloadCapacity,
    Generation,
    OwnerThreadId,
    // --- Reserved line (192..256) ---
    WriterWaiting,
    ProcessMissingCalls,
}

impl Word {
    /// 全ての語 (初期化と検査用)。
    pub const ALL: [Word; 21] = [
        Word::WriteCursor,
        Word::PayloadWritePosition,
        Word::DroppedEvents,
        Word::DroppedPayloadBytes,
        Word::DegradedEvents,
        Word::OverflowState,
        Word::ExpansionRequested,
        Word::LastWaitNs,
        Word::ReadCursor,
        Word::PayloadReadPosition,
        Word::HostWaiting,
        Word::RingStopped,
        Word::HostDrainCount,
        Word::Magic,
        Word::LayoutVersion,
        Word::DescriptorCapacity,
        Word::PayloadCapacity,
        Word::Generation,
        Word::OwnerThreadId,
        Word::WriterWaiting,
        Word::ProcessMissingCalls,
    ];

    /// バイトオフセット (コピーした Control を読むときに使う)。
    pub const fn offset(self) -> usize {
        use crate::layout::*;
        match self {
            Word::WriteCursor => OFF_WRITE_CURSOR,
            Word::PayloadWritePosition => OFF_PAYLOAD_WRITE_POSITION,
            Word::DroppedEvents => OFF_DROPPED_EVENTS,
            Word::DroppedPayloadBytes => OFF_DROPPED_PAYLOAD_BYTES,
            Word::DegradedEvents => OFF_DEGRADED_EVENTS,
            Word::OverflowState => OFF_OVERFLOW_STATE,
            Word::ExpansionRequested => OFF_EXPANSION_REQUESTED,
            Word::LastWaitNs => OFF_LAST_WAIT_NS,
            Word::ReadCursor => OFF_READ_CURSOR,
            Word::PayloadReadPosition => OFF_PAYLOAD_READ_POSITION,
            Word::HostWaiting => OFF_HOST_WAITING,
            Word::RingStopped => OFF_RING_STOPPED,
            Word::HostDrainCount => OFF_HOST_DRAIN_COUNT,
            Word::Magic => OFF_MAGIC,
            Word::LayoutVersion => OFF_LAYOUT_VERSION,
            Word::DescriptorCapacity => OFF_DESCRIPTOR_CAPACITY,
            Word::PayloadCapacity => OFF_PAYLOAD_CAPACITY,
            Word::Generation => OFF_GENERATION,
            Word::OwnerThreadId => OFF_OWNER_THREAD_ID,
            Word::WriterWaiting => OFF_WRITER_WAITING,
            Word::ProcessMissingCalls => OFF_PROCESS_MISSING_CALLS,
        }
    }

    const fn index(self) -> usize {
        self.offset() / 8
    }
}

// 全ての語が Control 内の 8 バイト境界にあり、互いに重ならない。
const _: () = {
    let mut i = 0;
    while i < Word::ALL.len() {
        let off = Word::ALL[i].offset();
        assert!(off / 8 != SLOT_REGISTRATION_WORD);
        assert!(off / 8 != READER_REGISTRATION_WORD);
        assert!(off.is_multiple_of(8) && off + 8 <= CONTROL_SIZE);
        let mut j = i + 1;
        while j < Word::ALL.len() {
            assert!(Word::ALL[j].offset() != off);
            j += 1;
        }
        i += 1;
    }
};

/// Control 領域への読み取り専用の借用。語は [`Word`] でしか指せない。
/// 書き込みは役割ごとの [`ProducerControl`] / [`ConsumerControl`] と、
/// 初期化は [`UninitializedRing`] にしかない (レビュー 2026-11-16 P1)。
#[derive(Clone, Copy)]
pub struct Control<'a> {
    words: &'a [AtomicU64; CONTROL_WORDS],
}

impl<'a> Control<'a> {
    /// プロセス内の原子語の配列を Control として読む (テスト・モデル用)。
    pub fn from_words(words: &'a [AtomicU64; CONTROL_WORDS]) -> Self {
        Self { words }
    }

    /// クレート内専用 (役割を持つ型と初期化、壊れた Control を作るテスト)。
    #[inline(always)]
    pub(crate) fn at(self, w: Word) -> &'a AtomicU64 {
        &self.words[w.index()]
    }
    #[inline(always)]
    pub fn load(self, w: Word, order: Ordering) -> u64 {
        self.at(w).load(order)
    }

    /// 書き手が空き待ち中かどうか (Host が space_available の通知要否を判断する)。
    #[inline]
    pub fn writer_waiting(self) -> bool {
        self.load(Word::WriterWaiting, Ordering::Acquire) != 0
    }
    /// Host 側: 掲示された欠落呼び出し数 (信用しない参考値。下限として使う)。
    #[inline]
    pub fn process_missing_calls(self) -> u64 {
        self.load(Word::ProcessMissingCalls, Ordering::Relaxed)
    }
    /// Magic が公開済みか (Host と同一の読み手述語)。
    pub fn is_published(self) -> bool {
        self.load(Word::Magic, Ordering::Acquire) == MAGIC
    }

    /// Control 全体を語ごとの原子読み (Acquire) で所有データへ写す。
    pub fn snapshot(self) -> [u8; CONTROL_SIZE] {
        let mut out = [0u8; CONTROL_SIZE];
        for (chunk, w) in out.as_chunks_mut::<8>().0.iter_mut().zip(self.words) {
            *chunk = w.load(Ordering::Acquire).to_le_bytes();
        }
        out
    }

    /// 損失カウンタ。
    pub fn loss_counters(self) -> crate::layout::LossCounters {
        crate::layout::LossCounters {
            dropped_events: self.load(Word::DroppedEvents, Ordering::Relaxed),
            dropped_payload_bytes: self.load(Word::DroppedPayloadBytes, Ordering::Relaxed),
            degraded_events: self.load(Word::DegradedEvents, Ordering::Relaxed),
        }
    }
}

/// 書き手 (Producer) だけが更新する語。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProducerWord {
    WriteCursor,
    PayloadWritePosition,
    DroppedEvents,
    DroppedPayloadBytes,
    DegradedEvents,
    OverflowState,
    ExpansionRequested,
    LastWaitNs,
    WriterWaiting,
}

impl ProducerWord {
    pub const fn word(self) -> Word {
        match self {
            Self::WriteCursor => Word::WriteCursor,
            Self::PayloadWritePosition => Word::PayloadWritePosition,
            Self::DroppedEvents => Word::DroppedEvents,
            Self::DroppedPayloadBytes => Word::DroppedPayloadBytes,
            Self::DegradedEvents => Word::DegradedEvents,
            Self::OverflowState => Word::OverflowState,
            Self::ExpansionRequested => Word::ExpansionRequested,
            Self::LastWaitNs => Word::LastWaitNs,
            Self::WriterWaiting => Word::WriterWaiting,
        }
    }
}

/// 読み手 (Consumer) だけが更新する語。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsumerWord {
    ReadCursor,
    PayloadReadPosition,
    RingStopped,
    HostDrainCount,
}

impl ConsumerWord {
    pub const fn word(self) -> Word {
        match self {
            Self::ReadCursor => Word::ReadCursor,
            Self::PayloadReadPosition => Word::PayloadReadPosition,
            Self::RingStopped => Word::RingStopped,
            Self::HostDrainCount => Word::HostDrainCount,
        }
    }
}

/// 書き手側の Control。[`ProducerWord`] と、書き手側の手続き (HostWaiting の
/// 消費、欠落数の掲示) だけを書ける。[`Shm::producer_control`] で得る。
#[derive(Clone, Copy)]
pub struct ProducerControl<'a> {
    ctl: Control<'a>,
}

impl<'a> std::ops::Deref for ProducerControl<'a> {
    type Target = Control<'a>;
    fn deref(&self) -> &Control<'a> {
        &self.ctl
    }
}

impl ProducerControl<'_> {
    #[inline(always)]
    pub fn store(self, w: ProducerWord, v: u64, order: Ordering) {
        self.ctl.at(w.word()).store(v, order)
    }
    /// 単調カウンタの加算 (Relaxed)。
    #[inline(always)]
    pub fn fetch_add(self, w: ProducerWord, v: u64) -> u64 {
        self.ctl.at(w.word()).fetch_add(v, Ordering::Relaxed)
    }
    /// Host が立てた HostWaiting を消費する。立っていれば true。
    #[inline(always)]
    pub fn take_host_waiting(self) -> bool {
        self.ctl.at(Word::HostWaiting).swap(0, Ordering::Relaxed) != 0
    }
    /// Agent 側: プロセス全体の欠落呼び出し数を掲示する (単調、P1-1)。
    #[inline]
    pub fn publish_process_missing_calls(self, total: u64) {
        self.ctl
            .at(Word::ProcessMissingCalls)
            .fetch_max(total, Ordering::Relaxed);
    }
}

/// 読み手側の Control。[`ConsumerWord`] と HostWaiting だけを書ける。
/// [`Shm::consumer_control`] で得る。
#[derive(Clone, Copy)]
pub struct ConsumerControl<'a> {
    ctl: Control<'a>,
}

impl<'a> std::ops::Deref for ConsumerControl<'a> {
    type Target = Control<'a>;
    fn deref(&self) -> &Control<'a> {
        &self.ctl
    }
}

impl ConsumerControl<'_> {
    #[inline(always)]
    pub fn store(self, w: ConsumerWord, v: u64, order: Ordering) {
        self.ctl.at(w.word()).store(v, order)
    }
    /// 単調カウンタの加算 (Relaxed)。
    #[inline(always)]
    pub fn fetch_add(self, w: ConsumerWord, v: u64) -> u64 {
        self.ctl.at(w.word()).fetch_add(v, Ordering::Relaxed)
    }
    /// Host が空き通知を待つ印を立てる / 下ろす。
    #[inline(always)]
    pub fn set_host_waiting(self, waiting: bool, order: Ordering) {
        self.ctl
            .at(Word::HostWaiting)
            .store(u64::from(waiting), order)
    }
}

/// 領域の写し (所有データ)。検証器へ渡す像や、壊した像を作るテストに使う。
/// 共有メモリではないので、語の読み書きは普通のバイト操作で足りる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    bytes: Vec<u8>,
}

impl Image {
    pub fn zeroed(len: usize) -> Self {
        Self {
            bytes: vec![0; len],
        }
    }
    pub fn copy_of(src: &[u8]) -> Self {
        Self {
            bytes: src.to_vec(),
        }
    }
    /// `cfg` の大きさで、[`UninitializedRing::publish`] と同じ Control を持つ像。
    pub fn initialized(cfg: RingConfig, generation: u64, owner_thread_id: u64) -> Self {
        let mut img = Self::zeroed(cfg.required_len());
        img.init(cfg, generation, owner_thread_id);
        img
    }
    /// Control を [`UninitializedRing::publish`] と同じ内容で書き直す。
    pub fn init(&mut self, cfg: RingConfig, generation: u64, owner_thread_id: u64) {
        let words: [AtomicU64; CONTROL_WORDS] = std::array::from_fn(|_| AtomicU64::new(0));
        let ctl = Control::from_words(&words);
        init_control(ctl, cfg, generation, owner_thread_id);
        self.bytes[..CONTROL_SIZE].copy_from_slice(&ctl.snapshot());
    }
    pub fn get(&self, w: Word) -> u64 {
        let off = w.offset();
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.bytes[off..off + 8]);
        u64::from_le_bytes(b)
    }
    pub fn set(&mut self, w: Word, v: u64) {
        let off = w.offset();
        self.bytes[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

impl std::ops::Deref for Image {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
impl std::ops::DerefMut for Image {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_rejects_regions_without_control() {
        assert_eq!(
            Shm::alloc_zeroed(16).unwrap_err(),
            ShmError::TooSmall { len: 16 }
        );
        assert!(Shm::alloc_zeroed(CONTROL_SIZE).is_ok());
    }

    #[test]
    fn copy_out_is_bounds_checked_and_never_covers_control() {
        let shm = Shm::alloc_zeroed(CONTROL_SIZE + 8).unwrap();
        // SAFETY: 誰も書いていない領域。
        unsafe {
            assert_eq!(
                shm.copy_out(CONTROL_SIZE..CONTROL_SIZE + 8).unwrap().len(),
                8
            );
            assert!(shm.copy_out(CONTROL_SIZE..CONTROL_SIZE + 9).is_none());
            // Control は原子操作でしか読まない (レビュー 2026-11-16 P0)。
            assert!(shm.copy_out(0..8).is_none());
            assert!(shm.copy_out(CONTROL_SIZE - 1..CONTROL_SIZE + 1).is_none());
            #[allow(clippy::reversed_empty_ranges)]
            let reversed = CONTROL_SIZE + 5..CONTROL_SIZE + 4;
            assert!(shm.copy_out(reversed).is_none());
        }
    }

    #[test]
    fn control_words_round_trip_through_snapshot() {
        let cfg = RingConfig::new(4, 64);
        let shm = UninitializedRing::alloc(cfg).unwrap().publish(7, 42);
        let producer = [
            ProducerWord::WriteCursor,
            ProducerWord::PayloadWritePosition,
            ProducerWord::DroppedEvents,
            ProducerWord::DroppedPayloadBytes,
            ProducerWord::DegradedEvents,
            ProducerWord::OverflowState,
            ProducerWord::ExpansionRequested,
            ProducerWord::LastWaitNs,
            ProducerWord::WriterWaiting,
        ];
        let consumer = [
            ConsumerWord::ReadCursor,
            ConsumerWord::PayloadReadPosition,
            ConsumerWord::RingStopped,
            ConsumerWord::HostDrainCount,
        ];
        // SAFETY: このテストが唯一の書き手かつ読み手。
        let (p, c) = unsafe { (shm.producer_control(), shm.consumer_control()) };
        for (i, w) in producer.into_iter().enumerate() {
            p.store(w, 100 + i as u64, Ordering::Relaxed);
        }
        for (i, w) in consumer.into_iter().enumerate() {
            c.store(w, 200 + i as u64, Ordering::Relaxed);
        }
        c.set_host_waiting(true, Ordering::Relaxed);
        p.publish_process_missing_calls(9);
        // SAFETY: 誰も書いていない。
        let snap = unsafe { shm.snapshot() };
        let img = Image::copy_of(&snap);
        for (i, w) in producer.into_iter().enumerate() {
            assert_eq!(img.get(w.word()), 100 + i as u64, "{w:?}");
        }
        for (i, w) in consumer.into_iter().enumerate() {
            assert_eq!(img.get(w.word()), 200 + i as u64, "{w:?}");
        }
        assert_eq!(img.get(Word::HostWaiting), 1);
        assert_eq!(img.get(Word::ProcessMissingCalls), 9);
        assert_eq!(img.get(Word::Magic), MAGIC);
        assert_eq!(img.get(Word::Generation), 7);
        assert_eq!(img.get(Word::OwnerThreadId), 42);
        assert!(p.take_host_waiting());
        assert!(!p.take_host_waiting());
    }

    /// 役割の語は互いに素で、Meta 行の語はどちらにも無い。全ての語は
    /// Producer / Consumer / HostWaiting・ProcessMissingCalls / Meta の
    /// どれか一つに属する (役割の取り違えを表の段階で防ぐ)。
    #[test]
    fn role_words_partition_control() {
        use std::collections::HashSet;
        let producer: HashSet<Word> = [
            ProducerWord::WriteCursor,
            ProducerWord::PayloadWritePosition,
            ProducerWord::DroppedEvents,
            ProducerWord::DroppedPayloadBytes,
            ProducerWord::DegradedEvents,
            ProducerWord::OverflowState,
            ProducerWord::ExpansionRequested,
            ProducerWord::LastWaitNs,
            ProducerWord::WriterWaiting,
        ]
        .map(ProducerWord::word)
        .into();
        let consumer: HashSet<Word> = [
            ConsumerWord::ReadCursor,
            ConsumerWord::PayloadReadPosition,
            ConsumerWord::RingStopped,
            ConsumerWord::HostDrainCount,
        ]
        .map(ConsumerWord::word)
        .into();
        let meta: HashSet<Word> = [
            Word::Magic,
            Word::LayoutVersion,
            Word::DescriptorCapacity,
            Word::PayloadCapacity,
            Word::Generation,
            Word::OwnerThreadId,
        ]
        .into();
        let handshake: HashSet<Word> = [Word::HostWaiting, Word::ProcessMissingCalls].into();
        let sets = [&producer, &consumer, &meta, &handshake];
        for (i, a) in sets.iter().enumerate() {
            for b in &sets[i + 1..] {
                assert!(a.is_disjoint(b));
            }
        }
        let all: HashSet<Word> = sets.iter().flat_map(|s| s.iter().copied()).collect();
        assert_eq!(all, Word::ALL.into_iter().collect());
    }

    #[test]
    fn misaligned_backing_is_refused() {
        struct Odd(Shm);
        // SAFETY: 実体は Shm の領域 + 1。長さは 1 減らす。
        unsafe impl Backing for Odd {
            fn base(&self) -> NonNull<u8> {
                NonNull::new(self.0.as_ptr().wrapping_add(1)).unwrap()
            }
            fn byte_len(&self) -> usize {
                self.0.len() - 1
            }
        }
        let inner = Shm::alloc_zeroed(CONTROL_SIZE * 2).unwrap();
        assert_eq!(
            Shm::new(Arc::new(Odd(inner))).unwrap_err(),
            ShmError::Misaligned
        );
    }
}
