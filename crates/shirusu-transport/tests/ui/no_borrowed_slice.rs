// 共有メモリを &[u8] として借りる API は無い (Snapshot は所有データ)。
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::RingConfig;

fn main() {
    let region = SharedRegion::new(RingConfig::new(4, 64), 1, 1).unwrap();
    let _bytes: &[u8] = region.shm().as_slice();
}
