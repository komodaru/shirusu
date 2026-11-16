// レビュー 2026-11-16 P1: 読み取り専用の Control からは書けず、初期化もできない。
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::{RingConfig, Word};
use std::sync::atomic::Ordering;

fn main() {
    let region = SharedRegion::new(RingConfig::new(4, 64), 1, 1).unwrap();
    region.control().store(Word::WriteCursor, 1, Ordering::Relaxed);
    region.control().init(RingConfig::new(4, 64), 2, 1);
}
