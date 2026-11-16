// レビュー 2026-11-16 P0: 共有領域の生コピーは safe から呼べない
// (Control の原子書き込みや書き手の書き込みと同時に呼べたため)。
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::RingConfig;

fn main() {
    let region = SharedRegion::new(RingConfig::new(4, 64), 1, 1).unwrap();
    let _ = region.shm().copy_out(256..264);
    let _ = region.snapshot();
}
