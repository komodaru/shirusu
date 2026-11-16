// レビュー 2026-11-16 P1: 読み手 (Host) は書き手の WriteCursor を書けない。
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::{ProducerWord, RingConfig};
use std::sync::atomic::Ordering;

fn main() {
    let region = SharedRegion::new(RingConfig::new(4, 64), 1, 1).unwrap();
    // SAFETY: compile-fail fixture.
    let host = unsafe { region.consumer_control() };
    host.store(ProducerWord::WriteCursor, 1, Ordering::Relaxed);
}
