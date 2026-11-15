// Reader は単一 Reader 前提を呼び出し側が約束するときだけ作れる。
use shirusu_transport::ring::{RingReader, SharedRegion};
use shirusu_transport::RingConfig;

fn main() {
    let cfg = RingConfig::new(4, 64);
    let region = SharedRegion::new(cfg, 1, 1).unwrap();
    let _reader = RingReader::new(region.shm().clone(), cfg);
}
