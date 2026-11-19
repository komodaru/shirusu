//! Bootstrap 経由の Mapping 登録（Phase 3）: 初期 Slot 0 の Agent が、
//! Host から配送された Mapping だけで記録できることの確認。
//! Agent DLL の初期化はこの経路で Ring を得る。

use shirusu_agent::{Agent, RecordResult};
use shirusu_transport::context::HostMonitor;
use shirusu_transport::expansion::DeliveredMapping;
use shirusu_transport::expansion::ExpansionDelivery;
use shirusu_transport::ring::{RingReader, SharedRegion, WriterPolicy};
use shirusu_transport::slot::SlotPool;
use shirusu_transport::RingConfig;
use std::sync::Arc;

#[test]
fn duplicate_registration_preserves_events_in_the_registered_mapping() {
    let cfg = RingConfig::new(64, 4096);
    // Host 側が作った共有領域（プロセス内では SharedRegion で代用）
    let host_region = SharedRegion::new(cfg, 0, 0).unwrap();
    let base = host_region.shm().clone();

    let pool = Arc::new(SlotPool::new(0));
    let agent =
        Agent::new(pool, cfg, Arc::new(HostMonitor::always_alive())).with_policy(WriterPolicy {
            max_wait: std::time::Duration::ZERO,
            ..Default::default()
        });
    // Slot が無い状態では記録できない
    assert_eq!(
        agent.record_enter(7, 1, 1, 1, 0, b"x"),
        RecordResult::NoSlot
    );
    // Bootstrap 相当: Mapping を 1 本登録する
    let delivery = ExpansionDelivery {
        len: cfg.required_len(),
        mapping: DeliveredMapping::InProcess(base.clone()),
        generation: shirusu_transport::EXPANDED_SLOT_GENERATION,
    };
    assert_eq!(agent.register_slot_mapping(delivery), Some(0));
    // 登録後はその Mapping へ記録でき、Host 側から読み戻せる
    assert!(matches!(
        agent.record_enter(7, 1, 1, 1, 0, b"args-bytes"),
        RecordResult::Written(_)
    ));
    let duplicate = ExpansionDelivery {
        len: cfg.required_len(),
        mapping: DeliveredMapping::InProcess(base.clone()),
        generation: shirusu_transport::EXPANDED_SLOT_GENERATION,
    };
    assert_eq!(agent.register_slot_mapping(duplicate), None);
    // SAFETY: the only reader of this Ring (SPSC).
    let mut reader = unsafe { RingReader::new(base.clone(), cfg) }.unwrap();
    let batch = reader.drain_all();
    assert!(batch.integrity_error.is_none());
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].payload, b"args-bytes");
    assert_eq!(batch.events[0].descriptor.call_id, 1);
}

#[test]
fn register_slot_mapping_rejects_bad_deliveries() {
    let cfg = RingConfig::new(16, 512);
    let pool = Arc::new(SlotPool::new(0));
    let agent = Agent::new(pool, cfg, Arc::new(HostMonitor::always_alive()));
    // 実体もハンドルも無い配送は DeliveredMapping で表せない (旧 API の
    // `base: None, handle_value: None` はコンパイルできない)。
    // 世代が違う配送
    let region = SharedRegion::new(cfg, 0, 0).unwrap();
    let bad_gen = ExpansionDelivery {
        len: cfg.required_len(),
        mapping: DeliveredMapping::InProcess(region.shm().clone()),
        generation: 9,
    };
    assert_eq!(agent.register_slot_mapping(bad_gen), None);
    // 長さ不足の配送
    let short = ExpansionDelivery {
        len: 16,
        mapping: DeliveredMapping::InProcess(region.shm().clone()),
        generation: shirusu_transport::EXPANDED_SLOT_GENERATION,
    };
    assert_eq!(agent.register_slot_mapping(short), None);
    // 長さの申告は正しくても、実体が Ring より小さい配送
    let small = SharedRegion::new(RingConfig::new(2, 16), 0, 0).unwrap();
    let lying = ExpansionDelivery {
        len: cfg.required_len(),
        mapping: DeliveredMapping::InProcess(small.shm().clone()),
        generation: shirusu_transport::EXPANDED_SLOT_GENERATION,
    };
    assert_eq!(agent.register_slot_mapping(lying), None);
}

/// P1-1: Slot を取れないスレッドの呼び出しは自分の Ring を持たない。
/// Agent はプロセス全体の欠落数を全 Mapping の Control へ掲示し、Host は
/// それを Capture Coverage の欠落として読む。否定キャッシュ中も欠落は
/// 1 件ずつ数え、Slot が空けば取得をやり直す。
#[test]
fn slot_exhaustion_is_published_to_host_visible_control() {
    let cfg = RingConfig::new(64, 4096);
    let host_region = SharedRegion::new(cfg, 0, 0).unwrap();
    let base = host_region.shm().clone();
    let pool = Arc::new(SlotPool::new(0));
    let agent = Agent::new(
        Arc::clone(&pool),
        cfg,
        Arc::new(HostMonitor::always_alive()),
    )
    .with_policy(WriterPolicy {
        max_wait: std::time::Duration::ZERO,
        ..Default::default()
    });
    let delivery = ExpansionDelivery {
        len: cfg.required_len(),
        mapping: DeliveredMapping::InProcess(base.clone()),
        generation: shirusu_transport::EXPANDED_SLOT_GENERATION,
    };
    assert_eq!(agent.register_slot_mapping(delivery), Some(0));
    // スレッド 7 が唯一の Slot を持つ。
    assert!(matches!(
        agent.record_enter(7, 1, 1, 1, 0, b"x"),
        RecordResult::Written(_)
    ));
    // スレッド 8 は別の OS スレッドから記録し、毎回 NoSlot になる。
    let a = Arc::new(agent);
    let a2 = Arc::clone(&a);
    std::thread::spawn(move || {
        for _ in 0..1000 {
            assert_eq!(a2.record_enter(8, 1, 1, 1, 0, b"y"), RecordResult::NoSlot);
        }
    })
    .join()
    .unwrap();
    assert_eq!(pool.missing_calls(), 1000, "every skipped call is counted");
    let shown = base.control().process_missing_calls();
    assert!(
        (1..=1000).contains(&shown),
        "the first miss is published at once: {shown}"
    );
    a.publish_missing_calls();
    assert_eq!(base.control().process_missing_calls(), 1000);
}
