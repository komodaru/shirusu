//! proptest_properties.rs と proptest_ring.rs が共有するリングの組み立て。

use shirusu_transport::clock::ManualClock;
use shirusu_transport::ring::*;
use shirusu_transport::{Clock, RingConfig};
use std::sync::Arc;
use std::time::Duration;

pub fn setup(desc: usize, pay: usize) -> (SharedRegion, RingWriter, RingReader) {
    let cfg = RingConfig::new(desc, pay);
    let region = SharedRegion::new(cfg, 1, 1).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new());
    // SAFETY: the only writer of this region (SPSC).
    let w = unsafe {
        region.writer(
            WriterPolicy {
                max_wait: Duration::ZERO,
                ..Default::default()
            },
            clock,
        )
    };
    // SAFETY: the only reader of this region (SPSC).
    let r = unsafe { region.reader() };
    (region, w, r)
}
