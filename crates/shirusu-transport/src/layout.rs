//! 共有メモリ上のレイアウト定義。
//! Control は「書き手が更新する値」と「読み手が更新する値」を別の 64B キャッシュラインに分離する。
//! 共有領域には生ポインタを置かず、すべての位置は論理オフセットで表す。
//! 共有メモリそのものへの操作は [`crate::shm`] の型 (`Shm` / `Control`) だけが
//! 持つ。ここにはオフセット (コピーした Control を読むため) と純粋な演算を置く。

pub const CACHE_LINE: usize = 64;
/// Control 領域の大きさ（4 キャッシュライン）。
pub const CONTROL_SIZE: usize = 256;
pub const MAGIC: u64 = 0x5348_4952_5553_5531; // "SHIRUSU1"
pub const LAYOUT_VERSION: u64 = 1;

// --- Writer line (offset 0..64): 書き手だけが更新する ---
pub const OFF_WRITE_CURSOR: usize = 0;
pub const OFF_PAYLOAD_WRITE_POSITION: usize = 8;
pub const OFF_DROPPED_EVENTS: usize = 16;
pub const OFF_DROPPED_PAYLOAD_BYTES: usize = 24;
pub const OFF_DEGRADED_EVENTS: usize = 32;
pub const OFF_OVERFLOW_STATE: usize = 40;
pub const OFF_EXPANSION_REQUESTED: usize = 48;
pub const OFF_LAST_WAIT_NS: usize = 56;
// --- Reader line (offset 64..128): 読み手だけが更新する ---
pub const OFF_READ_CURSOR: usize = 64;
pub const OFF_PAYLOAD_READ_POSITION: usize = 72;
pub const OFF_HOST_WAITING: usize = 80;
pub const OFF_RING_STOPPED: usize = 88;
pub const OFF_HOST_DRAIN_COUNT: usize = 96;
// --- Meta line (offset 128..192): 生成時に一度だけ書く。Host は信用せず自分の既知設定を使う ---
pub const OFF_MAGIC: usize = 128;
pub const OFF_LAYOUT_VERSION: usize = 136;
pub const OFF_DESCRIPTOR_CAPACITY: usize = 144;
pub const OFF_PAYLOAD_CAPACITY: usize = 152;
pub const OFF_GENERATION: usize = 160;
pub const OFF_OWNER_THREAD_ID: usize = 168;
// --- Reserved line (192..256) ---
// 192: 書き手が空き待ち中である意思表示（書き手だけが更新）。仕様の通知節:
// 待機の意思を示した後に全順序フェンスを打ち、空きを再確認する。Host はこの
// フラグを見て、空きができたときに space_available を SetEvent する。
pub const OFF_WRITER_WAITING: usize = 192;
// 200: プロセス全体で Slot を取れずに記録しなかった呼び出しの下限値
// (P1-1)。Agent は自分の全 Mapping へ単調に書き (fetch_max)、Host は全
// Ring の最大値を Capture Coverage の欠落として扱う。Slot を持てない
// スレッドには自分の Ring が無いため、他スレッドの Ring の Control を
// プロセス共通の掲示板として使う。Slot 再利用の初期化で 0 に戻っても、
// Host は観測した最大値を保持し、Agent は次の欠落で書き直す。
pub const OFF_PROCESS_MISSING_CALLS: usize = 200;

/// Ring の容量設定。フィールドは非公開で、構築時に必要容量を checked 算術で
/// 確定させる (レビュー 2026-11-15 P0-2: 公開フィールドと折り返す乗算の
/// 組み合わせで、16 バイトしか確保しない Region に 256 バイトの Control を
/// 書けた)。`required_len` は構築後に変わらず、`Layout` の上限
/// (`isize::MAX`) にも収まることが保証される。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingConfig {
    descriptor_capacity: usize,
    payload_capacity: usize,
    required_len: usize,
}

/// [`RingConfig::try_new`] の拒否理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    DescriptorCapacityTooSmall,
    PayloadCapacityTooSmall,
    /// 必要容量が `usize` または `isize::MAX` (確保の上限) を超える。
    TooLarge,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DescriptorCapacityTooSmall => f.write_str("descriptor capacity must be >= 2"),
            Self::PayloadCapacityTooSmall => f.write_str("payload capacity must be >= 16"),
            Self::TooLarge => f.write_str("ring does not fit in the address space"),
        }
    }
}
impl std::error::Error for ConfigError {}

// 容量は共有領域へ u64 で置く。usize が u64 に収まるので `as u64` は無損失。
const _: () = assert!(usize::BITS <= u64::BITS);

/// Descriptor 1 件の大きさ。
pub const DESCRIPTOR_SIZE: usize = 64;

impl RingConfig {
    /// 本番既定: Descriptor 4096 件 (=256KiB) / Payload 1MiB。
    pub const PRODUCTION: Self = Self::new(4096, 1024 * 1024);
    /// Opt-in burst capacity: 5 MiB + Control per Ring. This absorbs short
    /// producer bursts, but cannot guarantee loss-free sustained capture.
    pub const BURST: Self = Self::new(65536, 1024 * 1024);

    /// 容量を検証して設定を作る。必要容量は checked 算術で求める。
    pub const fn try_new(
        descriptor_capacity: usize,
        payload_capacity: usize,
    ) -> Result<Self, ConfigError> {
        if descriptor_capacity < 2 {
            return Err(ConfigError::DescriptorCapacityTooSmall);
        }
        if payload_capacity < 16 {
            return Err(ConfigError::PayloadCapacityTooSmall);
        }
        let Some(descriptors) = descriptor_capacity.checked_mul(DESCRIPTOR_SIZE) else {
            return Err(ConfigError::TooLarge);
        };
        let Some(payload_offset) = CONTROL_SIZE.checked_add(descriptors) else {
            return Err(ConfigError::TooLarge);
        };
        let Some(required_len) = payload_offset.checked_add(payload_capacity) else {
            return Err(ConfigError::TooLarge);
        };
        // Layout::from_size_align の上限 (整列 64 の切り上げ分を含む)。
        if required_len > isize::MAX as usize - CACHE_LINE {
            return Err(ConfigError::TooLarge);
        }
        Ok(Self {
            descriptor_capacity,
            payload_capacity,
            required_len,
        })
    }
    /// 定数とテスト用: 不正な容量なら panic する (未定義動作にはならない)。
    /// 外部入力から作るときは [`RingConfig::try_new`] を使う。
    pub const fn new(descriptor_capacity: usize, payload_capacity: usize) -> Self {
        match Self::try_new(descriptor_capacity, payload_capacity) {
            Ok(c) => c,
            Err(ConfigError::DescriptorCapacityTooSmall) => {
                panic!("descriptor capacity must be >= 2")
            }
            Err(ConfigError::PayloadCapacityTooSmall) => panic!("payload capacity must be >= 16"),
            Err(ConfigError::TooLarge) => panic!("ring does not fit in the address space"),
        }
    }
    /// 共有領域に置いた u64 の容量から作る (Host が受け取った値など)。
    pub fn try_from_u64(
        descriptor_capacity: u64,
        payload_capacity: u64,
    ) -> Result<Self, ConfigError> {
        let d = usize::try_from(descriptor_capacity).map_err(|_| ConfigError::TooLarge)?;
        let p = usize::try_from(payload_capacity).map_err(|_| ConfigError::TooLarge)?;
        Self::try_new(d, p)
    }
    pub const fn descriptor_capacity(&self) -> usize {
        self.descriptor_capacity
    }
    pub const fn payload_capacity(&self) -> usize {
        self.payload_capacity
    }
    /// 共有領域上の表現 (u64)。構築時に損失なく変換できることを確かめてある。
    pub const fn descriptor_capacity_u64(&self) -> u64 {
        self.descriptor_capacity as u64
    }
    pub const fn payload_capacity_u64(&self) -> u64 {
        self.payload_capacity as u64
    }
    pub const fn descriptors_offset(&self) -> usize {
        CONTROL_SIZE
    }
    pub const fn payload_offset(&self) -> usize {
        // 構築時に checked で確かめた和 (required_len - payload_capacity)。
        self.required_len - self.payload_capacity
    }
    pub const fn required_len(&self) -> usize {
        self.required_len
    }
}

/// 純粋な Cursor 演算（Kani / PBT の対象）。
#[inline]
pub fn occupancy(write_cursor: u64, read_cursor: u64) -> Option<u64> {
    write_cursor.checked_sub(read_cursor)
}
#[inline]
pub fn descriptor_free(write_cursor: u64, read_cursor: u64, capacity: u64) -> Option<u64> {
    capacity.checked_sub(occupancy(write_cursor, read_cursor)?)
}
#[inline]
pub fn slot_index(cursor: u64, capacity: u64) -> u64 {
    if capacity == 0 {
        0
    } else if capacity.is_power_of_two() {
        // Production capacities are powers of two: avoid a 64-bit division
        // (tens of cycles) on every published event. Same result as `%`.
        cursor & (capacity - 1)
    } else {
        cursor % capacity
    }
}
#[inline]
pub fn payload_index(position: u64, capacity: u64) -> u64 {
    slot_index(position, capacity)
}
#[inline]
pub fn usage_percent(used: u64, capacity: u64) -> u64 {
    used.saturating_mul(100)
        .checked_div(capacity)
        .unwrap_or(100)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LossCounters {
    pub dropped_events: u64,
    pub dropped_payload_bytes: u64,
    pub degraded_events: u64,
}
impl LossCounters {
    /// 前回値との差分（単調増加が前提。後退は破損として None）。
    pub fn diff_since(&self, prev: &Self) -> Option<Self> {
        Some(Self {
            dropped_events: self.dropped_events.checked_sub(prev.dropped_events)?,
            dropped_payload_bytes: self
                .dropped_payload_bytes
                .checked_sub(prev.dropped_payload_bytes)?,
            degraded_events: self.degraded_events.checked_sub(prev.degraded_events)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::assertions_on_constants)] // 定数の配置自体を固定するのが目的
    fn writer_and_reader_lines_are_separate() {
        assert!(OFF_WRITE_CURSOR / 64 != OFF_READ_CURSOR / 64);
        assert!(OFF_DROPPED_EVENTS / 64 == OFF_WRITE_CURSOR / 64);
        assert!(OFF_PAYLOAD_READ_POSITION / 64 == OFF_READ_CURSOR / 64);
        assert!(OFF_MAGIC / 64 >= 2);
    }
    #[test]
    #[allow(clippy::assertions_on_constants)] // 定数の配置自体を固定するのが目的
    fn writer_waiting_flag_lives_in_reserved_line() {
        // 書き手更新だが Writer line は満杯のため Reserved line を使う。
        // Reader line とは別キャッシュラインで、8 バイト境界にあること。
        assert!(OFF_WRITER_WAITING >= 192 && OFF_WRITER_WAITING < CONTROL_SIZE);
        assert_eq!(OFF_WRITER_WAITING % 8, 0);
        assert!(OFF_WRITER_WAITING / 64 != OFF_READ_CURSOR / 64);
        assert!(OFF_WRITER_WAITING / 64 != OFF_WRITE_CURSOR / 64);
    }
    #[test]
    fn cursor_math() {
        assert_eq!(occupancy(10, 3), Some(7));
        assert_eq!(occupancy(3, 10), None);
        assert_eq!(descriptor_free(10, 3, 16), Some(9));
        assert_eq!(slot_index(4097, 4096), 1);
        assert_eq!(usage_percent(1, 4), 25);
        assert_eq!(usage_percent(0, 0), 100);
    }
    #[test]
    fn production_config_sizes() {
        let c = RingConfig::PRODUCTION;
        assert_eq!(c.descriptor_capacity() * 64, 256 * 1024);
        assert_eq!(c.required_len(), 256 + 256 * 1024 + 1024 * 1024);
    }
    #[test]
    fn init_control_publishes_magic_last_staged() {
        // レビュー第5回 #4 の段階テスト: 書き手の途中段階ごとに読み手
        // 述語を回し、最終の Magic 公開より前は「初期化済み」に
        // ならないことを確認する。
        use crate::shm::{Shm, Word};
        use crate::validate::{validate_control, IntegrityError, KnownConfig};
        use std::sync::atomic::Ordering;
        let cfg = RingConfig::new(8, 256);
        let shm = Shm::alloc_zeroed(cfg.required_len()).unwrap();
        let ctl = shm.control();
        for w in Word::ALL {
            ctl.store(w, 0xDEAD_BEEF_DEAD_BEEF, Ordering::Relaxed);
        }
        // 段階 0: 未初期化 (ゴミ)。述語は偽。
        assert!(!ctl.is_published());
        // 段階 1: ゼロクリア + 全フィールド書き込み (公開前)。
        ctl.init_fields(cfg, 7, 42);
        assert_eq!(ctl.load(Word::LayoutVersion, Ordering::Relaxed), 1);
        assert_eq!(ctl.load(Word::DescriptorCapacity, Ordering::Relaxed), 8);
        assert_eq!(ctl.load(Word::Generation, Ordering::Relaxed), 7);
        assert_eq!(ctl.load(Word::OwnerThreadId, Ordering::Relaxed), 42);
        assert!(!ctl.is_published());
        let known = KnownConfig::new(cfg, cfg.required_len());
        assert_eq!(
            validate_control(&ctl.snapshot(), &known, None),
            Err(IntegrityError::BadMagic)
        );
        // 段階 2: Magic を Release で公開 (init の最終段と同一)。
        ctl.store(Word::Magic, MAGIC, Ordering::Release);
        assert!(ctl.is_published());
        let snap =
            validate_control(&ctl.snapshot(), &known, None).expect("initialized control validates");
        assert_eq!(snap.generation, 7);
        // 一括の init でも同じ到達状態になる。
        let shm2 = Shm::alloc_zeroed(cfg.required_len()).unwrap();
        shm2.control().init(cfg, 7, 42);
        assert!(shm2.control().is_published());
        assert_eq!(shm2.control().snapshot(), ctl.snapshot());
    }

    /// P0-2 (レビュー 2026-11-15): 旧実装では `RingConfig::new(2, usize::MAX - 367)`
    /// の必要容量が Release ビルドで 16 に折り返し、16 バイトの Region に
    /// 256 バイトの Control を書いた。
    #[test]
    fn overflowing_capacities_are_refused() {
        assert_eq!(
            RingConfig::try_new(2, usize::MAX - 367),
            Err(ConfigError::TooLarge)
        );
        assert_eq!(
            RingConfig::try_new(usize::MAX / 64 + 1, 16),
            Err(ConfigError::TooLarge)
        );
        assert_eq!(
            RingConfig::try_new(usize::MAX / 128, usize::MAX / 2),
            Err(ConfigError::TooLarge)
        );
        assert_eq!(
            RingConfig::try_new(1, 16),
            Err(ConfigError::DescriptorCapacityTooSmall)
        );
        assert_eq!(
            RingConfig::try_new(2, 15),
            Err(ConfigError::PayloadCapacityTooSmall)
        );
        assert_eq!(
            RingConfig::try_from_u64(u64::MAX, 16),
            Err(ConfigError::TooLarge)
        );
    }

    #[test]
    fn loss_counters_are_monotonic_diff() {
        let a = LossCounters {
            dropped_events: 5,
            dropped_payload_bytes: 10,
            degraded_events: 1,
        };
        let b = LossCounters {
            dropped_events: 9,
            dropped_payload_bytes: 10,
            degraded_events: 3,
        };
        assert_eq!(
            b.diff_since(&a),
            Some(LossCounters {
                dropped_events: 4,
                dropped_payload_bytes: 0,
                degraded_events: 2
            })
        );
        assert_eq!(a.diff_since(&b), None);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// 占有量は w >= read のときちょうど差であり、それ以外は None (後退検出)。
    #[kani::proof]
    fn occupancy_is_exact_difference_or_none() {
        let w: u64 = kani::any();
        let r: u64 = kani::any();
        match occupancy(w, r) {
            Some(d) => {
                assert!(w >= r);
                assert!(d == w - r);
            }
            None => assert!(w < r),
        }
    }

    /// 空き容量は容量を超えず、占有量と相補である (全折り返しケース)。
    #[kani::proof]
    fn descriptor_free_never_exceeds_capacity() {
        let w: u64 = kani::any();
        let r: u64 = kani::any();
        let cap: u64 = kani::any();
        if let Some(f) = descriptor_free(w, r, cap) {
            assert!(f <= cap);
            assert!(occupancy(w, r) == Some(cap - f));
        }
    }

    /// スロット位置は剰余そのもので、容量未満に収まる。
    /// 範囲は小さく制限する: 記号除算は CBMC のビットブラストでは幅に
    /// 強く依存して遅く、16 bit 幅でも 20 CPU 分を超えて解けなかった (実測)。
    /// 剰余の定義そのものは幅に依存しないため、小さい範囲で証明し、
    /// 任意幅での等価性は境界テスト (u64 極値) と PBT が担う。
    #[kani::proof]
    fn slot_index_is_remainder_and_in_range() {
        let c: u64 = kani::any();
        let cap: u64 = kani::any();
        kani::assume(c < 4096 && cap >= 1 && cap <= 64);
        let idx = slot_index(c, cap);
        assert!(idx == c % cap);
        assert!(idx < cap);
    }

    /// try_new は u128 で計算した真の長さと一致し、Layout の上限を超える
    /// 組み合わせ (桁あふれを含む) だけを TooLarge として拒む。
    /// 乗算 1 回と加算 2 回だけなので任意幅のまま解ける。
    #[kani::proof]
    fn try_new_matches_wide_arithmetic() {
        let d: usize = kani::any();
        let p: usize = kani::any();
        let wide = CONTROL_SIZE as u128 + d as u128 * DESCRIPTOR_SIZE as u128 + p as u128;
        let limit = (isize::MAX as usize - CACHE_LINE) as u128;
        match RingConfig::try_new(d, p) {
            Ok(cfg) => {
                assert!(d >= 2 && p >= 16);
                assert!(cfg.required_len() as u128 == wide);
                assert!(wide <= limit);
                assert!(cfg.payload_offset() as u128 + p as u128 == wide);
            }
            Err(ConfigError::DescriptorCapacityTooSmall) => assert!(d < 2),
            Err(ConfigError::PayloadCapacityTooSmall) => assert!(d >= 2 && p < 16),
            Err(ConfigError::TooLarge) => assert!(d >= 2 && p >= 16 && wide > limit),
        }
    }

    /// 容量 0 では除算せず 0 に潰れる (任意の Cursor 値で)。
    #[kani::proof]
    fn slot_index_zero_capacity_is_zero() {
        let c: u64 = kani::any();
        assert!(slot_index(c, 0) == 0);
    }
}
