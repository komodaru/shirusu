//! P1-1 の決定的 E2E: Slot を持てなかったスレッドが受け渡し口へ要求を置き、
//! 管理パスが空いた Slot で作った Ring を渡し、そのスレッドが次の記録で
//! 受け取って書く。取り消し (thread_exit) の後に作った Ring は渡らない。

use shirusu_agent::{Agent, RecordResult};
use shirusu_transport::context::HostMonitor;
use shirusu_transport::expansion::{DeliveredMapping, ExpansionDelivery};
use shirusu_transport::ring::{RingReader, SharedRegion, WriterPolicy};
use shirusu_transport::slot::SlotPool;
use shirusu_transport::{RingConfig, Shm};
use std::sync::mpsc;
use std::sync::Arc;

fn cfg() -> RingConfig {
    RingConfig::new(64, 4096)
}

fn agent() -> Arc<Agent> {
    let pool = Arc::new(SlotPool::new(0));
    Arc::new(
        Agent::new(pool, cfg(), Arc::new(HostMonitor::always_alive())).with_policy(WriterPolicy {
            max_wait: std::time::Duration::ZERO,
            ..Default::default()
        }),
    )
}

fn add_mapping(agent: &Agent) -> Shm {
    let region = SharedRegion::new(cfg(), 0, 0).unwrap();
    let shm = region.shm().clone();
    let delivery = ExpansionDelivery {
        len: cfg().required_len(),
        mapping: DeliveredMapping::InProcess(shm.clone()),
        generation: shirusu_transport::EXPANDED_SLOT_GENERATION,
    };
    agent.register_slot_mapping(delivery).expect("slot added");
    shm
}

/// 別の OS スレッドで、指示のたびに 1 回記録する (TLS を保つため同じスレッド)。
fn recorder(
    agent: Arc<Agent>,
    tid: u64,
) -> (mpsc::Sender<&'static [u8]>, mpsc::Receiver<RecordResult>) {
    let (go, steps) = mpsc::channel::<&'static [u8]>();
    let (done, results) = mpsc::channel();
    std::thread::spawn(move || {
        for payload in steps {
            done.send(agent.record_enter(tid, 1, 1, 1, 0, payload))
                .unwrap();
        }
    });
    (go, results)
}

fn step(
    go: &mpsc::Sender<&'static [u8]>,
    results: &mpsc::Receiver<RecordResult>,
    payload: &'static [u8],
) -> RecordResult {
    go.send(payload).unwrap();
    results.recv().unwrap()
}

#[test]
fn a_thread_without_a_slot_adopts_a_ring_built_for_it() {
    let agent = agent();
    let first = add_mapping(&agent);
    assert!(matches!(
        agent.record_enter(7, 1, 1, 1, 0, b"main"),
        RecordResult::Written(_)
    ));
    let (go, results) = recorder(Arc::clone(&agent), 8);
    assert_eq!(step(&go, &results, b"miss"), RecordResult::NoSlot);
    // 何も空いていなければ渡せない。
    assert_eq!(agent.serve_adoptions(), 0);
    // Mapping の登録は管理パスで、待っているスレッドへ Ring を作って渡す。
    let second = add_mapping(&agent);
    assert!(agent.thread_region(8).unwrap().same_region(&second));
    assert_eq!(agent.serve_adoptions(), 0, "served once");
    assert!(matches!(
        step(&go, &results, b"adopted"),
        RecordResult::Written(_)
    ));
    // 受け取った Ring は 2 本目の Mapping で、スレッド 8 の記録だけが入る。
    let mut reader = unsafe { RingReader::new(second.clone(), cfg()) }.unwrap();
    let batch = reader.drain_all();
    assert!(batch.integrity_error.is_none());
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].payload, b"adopted");
    assert!(agent.thread_region(7).unwrap().same_region(&first));
}

#[test]
fn an_exited_thread_is_not_handed_a_ring() {
    let agent = agent();
    add_mapping(&agent);
    assert!(matches!(
        agent.record_enter(7, 1, 1, 1, 0, b"main"),
        RecordResult::Written(_)
    ));
    let (go, results) = recorder(Arc::clone(&agent), 8);
    assert_eq!(step(&go, &results, b"miss"), RecordResult::NoSlot);
    // 要求を置いたスレッドが終わった: 要求は取り消され、空いた Slot は使わない。
    assert!(!agent.thread_exit(8));
    add_mapping(&agent);
    assert_eq!(agent.serve_adoptions(), 0);
    assert!(agent.thread_region(8).is_none());
}

#[test]
fn a_thread_that_registers_normally_drops_its_request() {
    let agent = agent();
    add_mapping(&agent);
    assert!(matches!(
        agent.record_enter(7, 1, 1, 1, 0, b"main"),
        RecordResult::Written(_)
    ));
    assert_eq!(
        agent.record_enter(8, 1, 1, 1, 0, b"miss"),
        RecordResult::NoSlot
    );
    // 登録が要求へ Ring を作って置く。受け取る前に、同じ ID の別スレッド
    // (このテストのスレッドの TLS は NoSlot のまま) が通常の登録でその
    // Ring を得る。置かれた分は取り消される。
    let second = add_mapping(&agent);
    let (go, results) = recorder(Arc::clone(&agent), 8);
    assert!(matches!(
        step(&go, &results, b"fresh"),
        RecordResult::Written(_)
    ));
    assert!(agent.thread_region(8).unwrap().same_region(&second));
    // 置いてあった要求は消えていて、もう誰にも作らない。
    add_mapping(&agent);
    assert_eq!(agent.serve_adoptions(), 0);
}
