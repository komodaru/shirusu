// 7c5a553 レビュー P3: 取り消した claim の古いハンドルで、同じスレッドが取り直した新しい
// claim を取り消せた。claim は複製できず、取り消しと確定が消費する。確定後の SlotHandle では
// 取り消せない。
use shirusu_transport::slot::SlotPool;

fn main() {
    let pool = SlotPool::new(1);
    let first = pool.claim(7).unwrap();
    let copy = first.clone();
    pool.cancel_claim(first);
    let second = pool.claim(7).unwrap();
    pool.cancel_claim(copy);
    let handle = second.confirm();
    pool.cancel_claim(handle);
}
