//! RingConfig::try_new の PBT (P0-2): checked 演算の結果を u128 の参照計算と
//! 照合する。境界 (容量の下限、乗算・加算の桁あふれ直前直後、Layout の上限)
//! に寄せた値を混ぜる。ケース数は SHIRUSU_PBT_CASES で上書き可能。

use proptest::prelude::*;
use shirusu_transport::layout::{CACHE_LINE, CONTROL_SIZE, DESCRIPTOR_SIZE};
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::{ConfigError, RingConfig, ShmError};

fn pbt_cases(default: u32) -> u32 {
    std::env::var("SHIRUSU_PBT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

const LIMIT: u128 = (isize::MAX as usize - CACHE_LINE) as u128;

fn reference(d: usize, p: usize) -> Result<usize, ConfigError> {
    if d < 2 {
        return Err(ConfigError::DescriptorCapacityTooSmall);
    }
    if p < 16 {
        return Err(ConfigError::PayloadCapacityTooSmall);
    }
    let wide = CONTROL_SIZE as u128 + d as u128 * DESCRIPTOR_SIZE as u128 + p as u128;
    if wide > LIMIT {
        return Err(ConfigError::TooLarge);
    }
    Ok(usize::try_from(wide).unwrap())
}

/// 桁あふれや上限の前後に寄せた値。
fn edgy() -> impl Strategy<Value = usize> {
    let near = |c: usize| (0usize..4).prop_map(move |k| c.wrapping_sub(k));
    let up = |c: usize| (0usize..4).prop_map(move |k| c.wrapping_add(k));
    prop_oneof![
        0usize..64,
        any::<usize>(),
        near(usize::MAX),
        near(usize::MAX / DESCRIPTOR_SIZE),
        up(usize::MAX / DESCRIPTOR_SIZE),
        near(isize::MAX as usize),
        near(isize::MAX as usize - CACHE_LINE - CONTROL_SIZE),
        near((isize::MAX as usize - CACHE_LINE - CONTROL_SIZE) / DESCRIPTOR_SIZE),
        up((isize::MAX as usize - CACHE_LINE - CONTROL_SIZE) / DESCRIPTOR_SIZE),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(pbt_cases(4096)))]

    #[test]
    fn try_new_matches_u128_reference(d in edgy(), p in edgy()) {
        let got = RingConfig::try_new(d, p);
        match (got, reference(d, p)) {
            (Ok(cfg), Ok(len)) => {
                prop_assert_eq!(cfg.required_len(), len);
                prop_assert_eq!(cfg.descriptor_capacity(), d);
                prop_assert_eq!(cfg.payload_capacity(), p);
                prop_assert_eq!(cfg.payload_offset() + p, len);
            }
            (Err(a), Err(b)) => prop_assert_eq!(a, b),
            (a, b) => prop_assert!(false, "try_new({d}, {p}) = {a:?}, reference {b:?}"),
        }
    }

    /// u64 経由の構築も、usize に収まる値では usize 版と同じ答えを返す。
    #[test]
    fn try_from_u64_agrees_with_try_new(d in any::<u64>(), p in any::<u64>()) {
        let got = RingConfig::try_from_u64(d, p);
        match (usize::try_from(d), usize::try_from(p)) {
            (Ok(du), Ok(pu)) => prop_assert_eq!(got, RingConfig::try_new(du, pu)),
            _ => prop_assert!(got.is_err()),
        }
    }
}

/// 回帰 (P0-2): 以前は桁あふれで小さい required_len になり、短い領域に
/// 大きな Ring を書き込めた。今は構築時に拒む。
#[test]
fn wrapping_capacities_no_longer_give_a_small_region() {
    // 旧実装: 256 + 2*64 + (usize::MAX - 300) が折り返して 83 になっていた。
    assert_eq!(
        RingConfig::try_new(2, usize::MAX - 300),
        Err(ConfigError::TooLarge)
    );
    // 旧実装: (usize::MAX/64 + 1) * 64 が 0 に折り返していた。
    assert_eq!(
        RingConfig::try_new(usize::MAX / 64 + 1, 16),
        Err(ConfigError::TooLarge)
    );
}

/// 上限内でも確保できない大きさは abort ではなく Err になる。
#[test]
fn unallocatable_region_is_an_error_not_an_abort() {
    // 上限ちょうど (64 bit では 2^63 付近、32 bit では 2 GiB 付近) は
    // 連続して確保できない。
    let payload = isize::MAX as usize - CACHE_LINE - CONTROL_SIZE - 2 * DESCRIPTOR_SIZE;
    let cfg = RingConfig::try_new(2, payload).unwrap();
    assert_eq!(cfg.required_len(), isize::MAX as usize - CACHE_LINE);
    assert!(matches!(
        SharedRegion::new(cfg, 1, 1),
        Err(ShmError::AllocFailed { .. })
    ));
}
