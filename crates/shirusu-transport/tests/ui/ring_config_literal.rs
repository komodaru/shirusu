// RingConfig は検査つきの構築 (try_new / new) でしか作れない。
use shirusu_transport::RingConfig;

fn main() {
    let _cfg = RingConfig {
        descriptor_capacity: usize::MAX / 64 + 1,
        payload_capacity: 16,
        required_len: 0,
    };
}
