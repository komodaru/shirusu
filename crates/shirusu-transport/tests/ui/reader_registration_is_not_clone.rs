// 7c5a553 レビュー P2-2: 読み手の登録は複製できない (同じ Mapping に二つの回収を作らせない)。
use shirusu_transport::ring::SharedRegion;
use shirusu_transport::RingConfig;

fn main() {
    let region = SharedRegion::new(RingConfig::new(4, 64), 1, 1).unwrap();
    let Some(reader) = region.shm().reserve_reader() else {
        return;
    };
    let _second = reader.clone();
}
