//! 外部レビュー 2026-11-16 P1 の E2E (管理モードの登録):
//!
//! - 記録するスレッドは初回でも登録表・Slot・Heap に触れず、要求を置いて
//!   管理スレッドが作った Ring を受け取る (共有ロックを取らない)。
//! - Slot を持てずに要求したまま終了したスレッドには、Slot が空いても Ring を
//!   作らない。作って置いた後に終了したスレッドの Ring は回収へ戻る。
//! - 受け取った後、一度も書かずに強制終了したスレッドの Ring も回収できる。
//!
//! 強制終了は Linux では「Drop を走らせずにスレッドを終える」形で表す
//! (ThreadKey は TLS の破棄で終了を知る)。

use shirusu_agent::{Agent, RecordResult, Registration};
use shirusu_transport::context::HostMonitor;
use shirusu_transport::ring::{RingReader, WriterPolicy};
use shirusu_transport::slot::SlotPool;
use shirusu_transport::RingConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

fn cfg() -> RingConfig {
    RingConfig::new(64, 4096)
}

fn agent(slots: usize) -> Arc<Agent> {
    Arc::new(
        Agent::new(
            Arc::new(SlotPool::new(slots)),
            cfg(),
            Arc::new(HostMonitor::always_alive()),
        )
        .with_policy(WriterPolicy {
            max_wait: Duration::ZERO,
            ..Default::default()
        })
        .with_registration(Registration::Managed),
    )
}

/// 管理スレッド: 止めるまで serve_managed を回す。結線されたスレッドを数える。
struct Manager {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Vec<u64>>>,
}
impl Manager {
    fn start(agent: &Arc<Agent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (Arc::clone(agent), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            let mut wired = Vec::new();
            while !s.load(Ordering::Acquire) {
                a.serve_managed(|tid, _| wired.push(tid));
                a.poll_reclaims();
                std::thread::park_timeout(Duration::from_millis(2));
            }
            wired
        });
        assert!(agent.set_manager_thread(thread.thread().clone()));
        Self {
            stop,
            thread: Some(thread),
        }
    }
    fn stop(mut self) -> Vec<u64> {
        self.stop.store(true, Ordering::Release);
        let t = self.thread.take().unwrap();
        t.thread().unpark();
        t.join().unwrap()
    }
}

/// 別の OS スレッドで、指示のたびに 1 回記録する (TLS を保つため同じスレッド)。
fn recorder(agent: Arc<Agent>, tid: u64) -> (mpsc::Sender<()>, mpsc::Receiver<RecordResult>) {
    let (go, steps) = mpsc::channel::<()>();
    let (done, results) = mpsc::channel();
    std::thread::spawn(move || {
        for () in steps {
            done.send(agent.record_enter(tid, 1, 1, 1, 0, b"x"))
                .unwrap();
        }
    });
    (go, results)
}

fn wait_until(mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn the_first_call_waits_for_the_manager_and_is_recorded() {
    let agent = agent(2);
    let manager = Manager::start(&agent);
    let a = Arc::clone(&agent);
    let (r, owned) = std::thread::spawn(move || {
        let r = a.record_enter(21, 1, 1, 1, 0, b"first");
        (r, a.current_thread_registered(21))
    })
    .join()
    .unwrap();
    assert!(matches!(r, RecordResult::Written(_)), "{r:?}");
    assert!(owned, "the Ring built by the manager is the thread's own");
    assert_eq!(agent.pool().missing_calls(), 0);
    assert_eq!(
        manager.stop(),
        vec![21],
        "wired by the manager before handing over"
    );
}

#[test]
fn without_a_manager_the_call_is_counted_missing_and_served_later() {
    let agent = agent(1);
    let (go, results) = recorder(Arc::clone(&agent), 22);
    go.send(()).unwrap();
    assert_eq!(results.recv().unwrap(), RecordResult::NoSlot);
    assert_eq!(agent.adoption_requests(), 1, "the request waits");
    assert!(
        agent.thread_region(22).is_none(),
        "the recorder built nothing"
    );
    assert_eq!(agent.serve_managed(|_, _| ()), 1);
    go.send(()).unwrap();
    assert!(matches!(results.recv().unwrap(), RecordResult::Written(_)));
    assert_eq!(agent.pool().missing_calls(), 1);
}

/// レビュー 2026-11-16 P1: Slot が無いまま要求して終了したスレッド。Slot が
/// 空いても、その要求には Ring を作らない。
#[test]
fn a_slot_less_requester_that_exited_gets_no_ring() {
    let agent = agent(1);
    // スレッド 1 が唯一の Slot を持つ (管理パスで作って渡す)。
    let (go1, res1) = recorder(Arc::clone(&agent), 1);
    go1.send(()).unwrap();
    assert_eq!(res1.recv().unwrap(), RecordResult::NoSlot);
    assert_eq!(agent.serve_managed(|_, _| ()), 1);
    go1.send(()).unwrap();
    assert!(matches!(res1.recv().unwrap(), RecordResult::Written(_)));
    // スレッド 2 は Slot が無く、要求を置いたまま終了する (終了通知なし)。
    let a = Arc::clone(&agent);
    std::thread::spawn(move || {
        assert_eq!(a.record_enter(2, 1, 1, 1, 0, b"x"), RecordResult::NoSlot);
    })
    .join()
    .unwrap();
    assert_eq!(agent.adoption_requests(), 1);
    // スレッド 1 が終わり、Host が読み終えると Slot が空く。
    drop(go1);
    let base = agent.thread_region(1).unwrap();
    assert!(agent.thread_exit(1));
    // SAFETY: the only reader of this Ring (SPSC).
    let mut reader = unsafe { RingReader::new(base, cfg()) }.unwrap();
    assert_eq!(reader.drain_all().events.len(), 1);
    wait_until(|| agent.poll_reclaims() > 0 || agent.pool().free_count() == 1);
    assert_eq!(agent.pool().free_count(), 1);
    // 管理パス: 終了した要求者の要求は取り消され、Slot は使われない。
    assert_eq!(agent.serve_managed(|_, _| ()), 0);
    assert_eq!(agent.adoption_requests(), 0);
    assert!(agent.thread_region(2).is_none());
    assert_eq!(agent.pool().free_count(), 1, "the Slot stays free");
    // 次に来たスレッドが使える。
    let (go3, res3) = recorder(Arc::clone(&agent), 3);
    go3.send(()).unwrap();
    assert_eq!(res3.recv().unwrap(), RecordResult::NoSlot);
    assert_eq!(agent.serve_managed(|_, _| ()), 1);
    go3.send(()).unwrap();
    assert!(matches!(res3.recv().unwrap(), RecordResult::Written(_)));
}

/// 作って置いた後、受け取る前に終了した要求者の Ring は回収へ戻る。
#[test]
fn a_ring_offered_to_a_requester_that_then_exited_is_reclaimed() {
    let agent = agent(1);
    let (tx, rx) = mpsc::channel::<()>();
    let a = Arc::clone(&agent);
    let requester = std::thread::spawn(move || {
        assert_eq!(a.record_enter(5, 1, 1, 1, 0, b"x"), RecordResult::NoSlot);
        rx.recv().unwrap(); // the manager serves now; then exit without taking
    });
    wait_until(|| agent.adoption_requests() == 1);
    assert_eq!(agent.serve_managed(|_, _| ()), 1);
    assert_eq!(agent.pool().free_count(), 0, "built and offered");
    tx.send(()).unwrap();
    requester.join().unwrap();
    // 次の管理パスが要求者の終了を見て Ring を回収へ送る。
    assert_eq!(agent.serve_managed(|_, _| ()), 0);
    assert!(agent.thread_region(5).is_none(), "retired");
    wait_until(|| {
        agent.poll_reclaims();
        agent.pool().free_count() == 1
    });
}

/// 受け取ったが一度も書かずに強制終了したスレッド (書き手は未取得)。要求者の
/// 鍵を管理パスが書き手へ割り当てるので、終了を検出して回収できる。
#[test]
fn a_requester_terminated_before_its_first_write_is_reaped() {
    let agent = agent(1);
    let a = Arc::clone(&agent);
    let (tx, rx) = mpsc::channel::<()>();
    let t = std::thread::spawn(move || {
        assert_eq!(a.record_enter(6, 1, 1, 1, 0, b"x"), RecordResult::NoSlot);
        rx.recv().unwrap();
        // 受け取ったところで (書く前に) 終了する。
        assert!(a.thread_begin(6).is_some());
    });
    wait_until(|| agent.adoption_requests() == 1);
    assert_eq!(agent.serve_managed(|_, _| ()), 1);
    tx.send(()).unwrap();
    t.join().unwrap();
    assert_eq!(agent.reap_terminated_threads(), vec![6]);
    wait_until(|| {
        agent.poll_reclaims();
        agent.pool().free_count() == 1
    });
}

/// 多数のスレッドが同時に初回の記録をしても、管理スレッドがそれぞれに Ring を
/// 渡し、Slot の数を超えた分は欠落として数えるだけで止まらない。
#[test]
fn many_first_calls_are_served_or_counted_missing() {
    let agent = agent(4);
    let manager = Manager::start(&agent);
    let results: Vec<RecordResult> = (0..12u64)
        .map(|i| {
            let a = Arc::clone(&agent);
            std::thread::spawn(move || {
                let r = a.record_enter(100 + i, 1, 1, 1, 0, b"x");
                // Hold the Slot until every thread has tried.
                std::thread::sleep(Duration::from_millis(300));
                r
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let written = results
        .iter()
        .filter(|r| matches!(r, RecordResult::Written(_)))
        .count();
    let missing = results
        .iter()
        .filter(|r| **r == RecordResult::NoSlot)
        .count();
    assert_eq!(written + missing, 12);
    assert!((1..=4).contains(&written), "{written}");
    manager.stop();
}
