//! Slot Pool の単体テスト (slot.rs は loom 側からも読み込むので、std の原子で
//! 動かすテストはここに置く)。
use shirusu_transport::slot::*;
use shirusu_transport::RingConfig;
use std::sync::atomic::Ordering;
#[test]
fn full_lifecycle_with_generation() {
    let pool = SlotPool::new(2);
    let h = pool.claim(111).unwrap().confirm();
    assert_eq!(pool.slot(h.index).state(), SlotState::Active);
    assert!(pool.begin_detach(&h));
    // 終了確認と回収完了の両方が無ければ再利用できない
    assert!(!pool.host_confirm_ended_and_drained(&h, false, true));
    assert!(!pool.host_confirm_ended_and_drained(&h, true, false));
    assert!(pool.host_confirm_ended_and_drained(&h, true, true));
    assert!(pool.host_reclaim(&h));
    let h2 = pool.claim(222).unwrap().confirm();
    assert_eq!(h2.index, h.index);
    assert_eq!(h2.generation, h.generation + 1);
    // 古い handle はもう使えない
    assert!(!pool.begin_detach(&h));
}
#[test]
fn exhaustion_counts_coverage_loss() {
    let pool = SlotPool::new(1);
    assert!(pool.claim(1).is_some());
    assert!(pool.claim(2).is_none());
    assert_eq!(pool.missing_calls(), 1);
}
#[test]
fn low_watermark_requests_expansion_only() {
    let pool = SlotPool::with_low_watermark(10, 8);
    pool.claim(1).unwrap().confirm();
    pool.claim(2).unwrap().confirm();
    pool.claim(3).unwrap().confirm(); // free becomes 7 < 8
    assert_eq!(pool.expansion_request_count.load(Ordering::Relaxed), 1);
    assert!(pool.take_expansion_request());
    assert!(!pool.take_expansion_request());
}
#[test]
fn detach_calls_are_counted_not_recorded() {
    let pool = SlotPool::new(1);
    let h = pool.claim(9).unwrap().confirm();
    assert!(!pool.note_call_after_detach(&h));
    pool.begin_detach(&h);
    assert!(pool.note_call_after_detach(&h));
    assert_eq!(pool.missing_calls(), 1);
}
#[test]
fn add_slot_starts_preparing_with_generation_zero() {
    // A70 + レビュー #4 の #10: 追加 Slot は generation 0・PREPARING
    // (取得不能) から始まり、既存 Slot には影響しない。公開するまで
    // claim はこの Slot を飛ばす。
    let pool = SlotPool::new(2);
    let h = pool.claim(111).unwrap().confirm();
    assert_eq!(pool.capacity(), 2);
    let idx = pool.add_slot().unwrap();
    assert_eq!(idx, 2);
    assert_eq!(pool.capacity(), 3);
    assert_eq!(pool.slot(idx).state(), SlotState::Preparing);
    assert_eq!(pool.slot(idx).generation(), 0);
    assert_eq!(pool.slot(idx).owner_thread_id(), 0);
    // 既存 Slot の generation/owner は不変。
    assert_eq!(pool.slot(h.index).generation(), h.generation);
    assert_eq!(pool.slot(h.index).owner_thread_id(), 111);
    assert_eq!(pool.slot(h.index).state(), SlotState::Active);
    // 未公開の間は claim できない (空き数にも数えない)。
    let h2 = pool.claim(222).unwrap().confirm();
    assert_eq!(h2.index, 1, "free initial slot is claimed first");
    assert!(pool.claim(333).is_none(), "preparing slot is not claimable");
    assert_eq!(pool.free_count(), 0);
    // 公開後は通常どおり claim できる。
    assert!(pool.publish_slot(idx));
    assert_eq!(pool.slot(idx).state(), SlotState::Free);
    let h3 = pool.claim(333).unwrap().confirm();
    assert_eq!(h3.index, idx);
    assert_eq!(h3.generation, 0);
}
#[test]
fn publish_slot_only_from_preparing() {
    let pool = SlotPool::new(1);
    // 初期 Slot は FREE であって PREPARING ではないので公開できない。
    assert!(!pool.publish_slot(0));
    assert_eq!(pool.slot(0).state(), SlotState::Free);
    let idx = pool.add_slot().unwrap();
    assert!(pool.publish_slot(idx));
    // 二重公開はできない。
    assert!(!pool.publish_slot(idx));
    // 範囲外 index は静かに false。
    assert!(!pool.publish_slot(999));
}
#[test]
fn custom_ring_size_controls_expansion_budget() {
    for config in [RingConfig::new(16, 512), RingConfig::BURST] {
        let bytes = config.required_len() as u64;
        let pool = SlotPool::new(1)
            .with_ring_config(config)
            .with_memory_limit(2 * bytes - 1);
        assert_eq!(pool.total_memory_bytes(), bytes);
        assert!(!pool.can_add_slot());
        assert_eq!(pool.add_slot(), None);
        let pool = SlotPool::new(1)
            .with_memory_limit(2 * bytes)
            .with_ring_config(config);
        assert_eq!(pool.add_slot(), Some(1));
        assert_eq!(pool.total_memory_bytes(), 2 * bytes);
        assert!(!pool.can_add_slot());
        assert_eq!(pool.add_slot(), None);
    }
}
#[test]
fn add_slot_respects_memory_limit() {
    // 総メモリ上限を超える場合は追加せず、既存 Slot は不変。
    let pool = SlotPool::new(2).with_memory_limit(2 * SLOT_MEMORY_BYTES);
    assert!(!pool.can_add_slot());
    assert_eq!(pool.add_slot(), None);
    assert_eq!(pool.capacity(), 2);
    // 1 本分の余裕があれば追加できる。
    let roomy = SlotPool::new(2).with_memory_limit(3 * SLOT_MEMORY_BYTES);
    assert!(roomy.can_add_slot());
    assert_eq!(roomy.add_slot(), Some(2));
    assert!(!roomy.can_add_slot());
    assert_eq!(roomy.add_slot(), None);
    assert_eq!(roomy.capacity(), 3);
}
#[test]
fn added_slot_full_lifecycle_and_reuse() {
    // 追加 Slot も初期 Slot と同じ生命環 (DETACH→回収→再利用) をたどる。
    // 再利用では generation が 0→1 に進む。
    let pool = SlotPool::new(1);
    let idx = pool.add_slot().unwrap();
    assert!(pool.publish_slot(idx));
    let h = pool.claim(7).unwrap().confirm();
    assert_eq!(h.index, 0, "initial slot is claimed first");
    let h2 = pool.claim(8).unwrap().confirm();
    assert_eq!(h2.index, idx);
    assert_eq!(h2.generation, 0);
    assert!(pool.begin_detach(&h2));
    assert!(pool.host_confirm_ended_and_drained(&h2, true, true));
    assert!(pool.host_reclaim(&h2));
    assert_eq!(pool.slot(idx).state(), SlotState::Free);
    assert_eq!(pool.slot(idx).generation(), 1);
    let h3 = pool.claim(9).unwrap().confirm();
    assert_eq!(h3.index, idx);
    assert_eq!(h3.generation, 1);
}
#[test]
fn add_slot_after_exhaustion_unblocks_claim() {
    let pool = SlotPool::new(1);
    assert!(pool.claim(1).is_some());
    assert!(pool.claim(2).is_none());
    assert_eq!(pool.add_slot(), Some(1));
    // 未公開の間は不足のまま。公開して初めて claim が通る。
    assert!(pool.claim(2).is_none());
    assert!(pool.publish_slot(1));
    let h = pool.claim(2).unwrap().confirm();
    assert_eq!(h.index, 1);
    assert_eq!(h.generation, 0);
}
#[test]
fn memory_limit_gates_expansion() {
    // 本番既定 1 Slot = 1,310,976 B。32 Slot で約 40MiB。
    assert_eq!(SLOT_MEMORY_BYTES, 1_310_976);
    let pool = SlotPool::production().with_memory_limit(40 * 1024 * 1024);
    assert_eq!(pool.total_memory_bytes(), 32 * SLOT_MEMORY_BYTES);
    assert!(!pool.can_add_slot()); // 33 Slot 目で 40MiB を超える
    let roomy = SlotPool::production().with_memory_limit(100 * 1024 * 1024);
    assert!(roomy.can_add_slot());
    assert!(SlotPool::production().can_add_slot()); // 上限なし
}
