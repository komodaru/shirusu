//! boundary.rs と matrix_ring.rs が共有する、SPSC リングの組み立てと 1 イベントの入力。

use shirusu_transport::clock::ManualClock;
use shirusu_transport::descriptor::{CaptureState, EventKind};
use shirusu_transport::ring::*;
use shirusu_transport::{Clock, RingConfig};
use std::sync::Arc;

pub fn setup(
    desc: usize,
    pay: usize,
    policy: WriterPolicy,
) -> (SharedRegion, RingWriter, RingReader, Arc<ManualClock>) {
    let cfg = RingConfig::new(desc, pay);
    let region = SharedRegion::new(cfg, 1, 1).unwrap();
    let clock = Arc::new(ManualClock::new());
    // SAFETY: the only writer of this region (SPSC).
    let w = unsafe { region.writer(policy, clock.clone() as Arc<dyn Clock>) };
    // SAFETY: the only reader of this region (SPSC).
    let r = unsafe { region.reader() };
    (region, w, r, clock)
}

pub fn input(kind: EventKind, call: u64, payload: &[u8]) -> EventInput<'_> {
    EventInput {
        kind,
        api_id: 10,
        schema_id: 20,
        call_id: call,
        parent_call_id: 0,
        payload,
        capture_state: CaptureState::Captured,
        global_order: 0,
    }
}
