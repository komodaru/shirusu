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

use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::layout::{RingConfig, CONTROL_SIZE, LAYOUT_VERSION, MAGIC};

/// Control の 64 bit 語の数。
pub const CONTROL_WORDS: usize = CONTROL_SIZE / 8;
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

impl Shm {
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

    /// 同じ実体かどうか。
    pub fn same_region(&self, other: &Shm) -> bool {
        Arc::ptr_eq(&self.keep, &other.keep)
    }

    /// `range` のバイトを所有データへ写す。範囲外は None。
    ///
    /// 書き手が同時に書いている範囲を写すと、その部分は書きかけの値になり
    /// うる (共有メモリの読み手は写した値を検証してから使う)。
    pub fn copy_out(&self, range: Range<usize>) -> Option<Vec<u8>> {
        if range.start > range.end || range.end > self.len {
            return None;
        }
        let mut out = vec![0u8; range.end - range.start];
        // SAFETY: 範囲は領域内。写し先は新しい Vec で重ならない。参照は作らない。
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.as_ptr().add(range.start),
                out.as_mut_ptr(),
                out.len(),
            );
        }
        Some(out)
    }

    /// 領域全体の写し。Control は原子読みで写す。
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = self
            .copy_out(CONTROL_SIZE..self.len)
            .map(|rest| {
                let mut v = Vec::with_capacity(self.len);
                v.resize(CONTROL_SIZE, 0);
                v.extend_from_slice(&rest);
                v
            })
            .unwrap_or_default();
        out[..CONTROL_SIZE].copy_from_slice(&self.control().snapshot());
        out
    }
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
        assert!(off.is_multiple_of(8) && off + 8 <= CONTROL_SIZE);
        let mut j = i + 1;
        while j < Word::ALL.len() {
            assert!(Word::ALL[j].offset() != off);
            j += 1;
        }
        i += 1;
    }
};

/// Control 領域への借用。語は [`Word`] でしか指せない。
#[derive(Clone, Copy)]
pub struct Control<'a> {
    words: &'a [AtomicU64; CONTROL_WORDS],
}

impl<'a> Control<'a> {
    /// プロセス内の原子語の配列を Control として使う (テスト・モデル用)。
    pub fn from_words(words: &'a [AtomicU64; CONTROL_WORDS]) -> Self {
        Self { words }
    }

    #[inline(always)]
    fn at(self, w: Word) -> &'a AtomicU64 {
        &self.words[w.index()]
    }
    #[inline(always)]
    pub fn load(self, w: Word, order: Ordering) -> u64 {
        self.at(w).load(order)
    }
    #[inline(always)]
    pub fn store(self, w: Word, v: u64, order: Ordering) {
        self.at(w).store(v, order)
    }
    #[inline(always)]
    pub fn swap(self, w: Word, v: u64, order: Ordering) -> u64 {
        self.at(w).swap(v, order)
    }
    /// 単調カウンタの加算 (Relaxed)。
    #[inline(always)]
    pub fn fetch_add(self, w: Word, v: u64) -> u64 {
        self.at(w).fetch_add(v, Ordering::Relaxed)
    }

    /// 書き手が空き待ち中かどうか (Host が space_available の通知要否を判断する)。
    #[inline]
    pub fn writer_waiting(self) -> bool {
        self.load(Word::WriterWaiting, Ordering::Acquire) != 0
    }
    /// Agent 側: プロセス全体の欠落呼び出し数を掲示する (単調、P1-1)。
    #[inline]
    pub fn publish_process_missing_calls(self, total: u64) {
        self.at(Word::ProcessMissingCalls)
            .fetch_max(total, Ordering::Relaxed);
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

    /// Control を初期化する (生成者のみが呼ぶ)。
    ///
    /// 公開順序 (レビュー第5回 #4): Magic は**最後**に `Release` で書く。
    /// それ以外の全フィールド (ゼロクリア・版数・容量・Generation・Owner) を
    /// 先に書き終えてから Magic を公開することで、Magic を `Acquire` で読んだ
    /// Host は初期化済みの全フィールドを見る。
    pub fn init(self, cfg: RingConfig, generation: u64, owner_thread_id: u64) {
        self.init_fields(cfg, generation, owner_thread_id);
        self.store(Word::Magic, MAGIC, Ordering::Release);
    }

    /// [`Control::init`] のうち Magic 以外 (公開前の段階テスト用に分離)。
    pub fn init_fields(self, cfg: RingConfig, generation: u64, owner_thread_id: u64) {
        for w in self.words {
            w.store(0, Ordering::Relaxed);
        }
        self.store(Word::LayoutVersion, LAYOUT_VERSION, Ordering::Relaxed);
        self.store(
            Word::DescriptorCapacity,
            cfg.descriptor_capacity_u64(),
            Ordering::Relaxed,
        );
        self.store(
            Word::PayloadCapacity,
            cfg.payload_capacity_u64(),
            Ordering::Relaxed,
        );
        self.store(Word::Generation, generation, Ordering::Relaxed);
        self.store(Word::OwnerThreadId, owner_thread_id, Ordering::Relaxed);
    }

    /// Control 全体を語ごとの原子読み (Acquire) で所有データへ写す。
    pub fn snapshot(self) -> [u8; CONTROL_SIZE] {
        let mut out = [0u8; CONTROL_SIZE];
        for (chunk, w) in out.chunks_exact_mut(8).zip(self.words) {
            chunk.copy_from_slice(&w.load(Ordering::Acquire).to_le_bytes());
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
    /// `cfg` の大きさで、[`Control::init`] と同じ Control を持つ像。
    pub fn initialized(cfg: RingConfig, generation: u64, owner_thread_id: u64) -> Self {
        let mut img = Self::zeroed(cfg.required_len());
        img.init(cfg, generation, owner_thread_id);
        img
    }
    /// Control を [`Control::init`] と同じ内容で書き直す。
    pub fn init(&mut self, cfg: RingConfig, generation: u64, owner_thread_id: u64) {
        let words: [AtomicU64; CONTROL_WORDS] = std::array::from_fn(|_| AtomicU64::new(0));
        let ctl = Control::from_words(&words);
        ctl.init(cfg, generation, owner_thread_id);
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
    fn copy_out_is_bounds_checked() {
        let shm = Shm::alloc_zeroed(CONTROL_SIZE + 8).unwrap();
        assert_eq!(
            shm.copy_out(0..CONTROL_SIZE + 8).unwrap().len(),
            CONTROL_SIZE + 8
        );
        assert!(shm.copy_out(0..CONTROL_SIZE + 9).is_none());
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = 5..4;
        assert!(shm.copy_out(reversed).is_none());
    }

    #[test]
    fn control_words_round_trip_through_snapshot() {
        let shm = Shm::alloc_zeroed(CONTROL_SIZE).unwrap();
        for (i, w) in Word::ALL.into_iter().enumerate() {
            shm.control().store(w, i as u64 + 1, Ordering::Relaxed);
        }
        let snap = shm.snapshot();
        for (i, w) in Word::ALL.into_iter().enumerate() {
            let off = w.offset();
            let v = u64::from_le_bytes(snap[off..off + 8].try_into().unwrap());
            assert_eq!(v, i as u64 + 1, "{w:?}");
        }
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
