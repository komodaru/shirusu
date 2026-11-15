// Control の語は閉じた列挙 Word でしか指せない (任意の offset は通らない)。
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::RingConfig;
use std::sync::atomic::Ordering;

fn main() {
    let region = SharedRegion::new(RingConfig::new(4, 64), 1, 1).unwrap();
    let _ = region.control().load(4096usize, Ordering::Acquire);
}
