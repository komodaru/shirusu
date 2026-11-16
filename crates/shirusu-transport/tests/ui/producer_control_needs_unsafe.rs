// レビュー 2026-11-16 P1: 役割付きの Control は unsafe でしか得られない。
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::RingConfig;

fn main() {
    let region = SharedRegion::new(RingConfig::new(4, 64), 1, 1).unwrap();
    let _ = region.shm().producer_control();
    let _ = region.shm().consumer_control();
}
