// 7c5a553 レビュー P3: 取り消した claim の古いハンドルで、同じスレッドが取り直した新しい
// claim を取り消せた。claim は複製できず、取り消しと確定が消費する。確定後の SlotHandle では
// 取り消せない。d4fc3c0 レビュー 2.2: 取り消しは発行元の Pool に対してだけ (別の Pool を渡す
// 経路がない)。
use shirusu_transport::slot::SlotPool;

fn main() {
    let pool = SlotPool::new(1);
    let other = SlotPool::new(1);
    let first = pool.claim(7).unwrap();
    let copy = first.clone();
    first.cancel();
    let second = pool.claim(7).unwrap();
    copy.cancel();
    let handle = second.confirm();
    handle.cancel();
    let theirs = other.claim(7).unwrap();
    pool.cancel_claim(theirs);
}
